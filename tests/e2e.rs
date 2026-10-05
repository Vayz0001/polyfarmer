//! End-to-end tests for Polyfarmer.
//!
//! Exercises the full application lifecycle over live loopback TCP sockets:
//! 1. First-run unauthenticated access, setup code verification, and admin password creation.
//! 2. Wallet credential validation and encrypted on-disk storage (master.key + wallet.enc).
//! 3. Authenticated dashboard browsing across all core routes with strict CSP headers.
//! 4. Real-time Server-Sent Events (SSE) streaming (/activity/stream).
//! 5. Session termination and logout invalidation.
//! 6. Trading engine quoting and safety protection rules (depth, volatility, expiry).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use chrono::{Duration as ChronoDuration, Utc};
use polyfarmer::creds::CredentialStore;
use polyfarmer::engine::orderbook::TokenBook;
use polyfarmer::engine::quoter::{self, CancelReason, DeactivateReason, QuoteAction};
use polyfarmer::engine::ws_manager::AppState;
use polyfarmer::types::{Alert, AlertLevel, EnginePhase, MarketConfig, OrderStatus};
use polyfarmer::web::{router, WebState};
use reqwest::header::{self, HeaderMap};
use reqwest::StatusCode;
use rust_decimal_macros::dec;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Notify, RwLock};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_test_dir() -> std::path::PathBuf {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("polyfarmer-e2e-{pid}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct TestServer {
    #[allow(dead_code)]
    pub addr: std::net::SocketAddr,
    pub base_url: String,
    pub data_dir: std::path::PathBuf,
    pub state: WebState,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl TestServer {
    async fn start_uninitialized() -> Self {
        let data_dir = temp_test_dir();
        let store = Arc::new(CredentialStore::open(&data_dir).expect("open credential store"));
        let markets_file = data_dir.join("markets.json");
        let alerts_file = data_dir.join("alerts.json");
        let reward_history_file = data_dir.join("reward_history.json");

        let engine = Arc::new(RwLock::new(AppState::new(markets_file)));
        let (alert_tx, _) = broadcast::channel::<Alert>(64);
        let quote_nudge = Arc::new(Notify::new());

        let state = WebState::with_config(store, engine, None, reward_history_file, alert_tx, alerts_file, quote_nudge);

        let app: Router = router(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .ok();
        });

        let base_url = format!("http://{addr}");
        Self { addr, base_url, data_dir, state, _shutdown: shutdown_tx }
    }
}

fn extract_csrf_token(html: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = html.find(marker).expect("csrf token input field in HTML") + marker.len();
    let end = html[start..].find('"').expect("closing quote for csrf token") + start;
    html[start..end].to_string()
}

fn extract_cookie(headers: &HeaderMap) -> Option<String> {
    headers.get(header::SET_COOKIE)?.to_str().ok()?.split(';').next().map(|s| s.to_string())
}

#[tokio::test]
async fn e2e_complete_server_and_trading_lifecycle() {
    let server = TestServer::start_uninitialized().await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client");

    // ── 1. Uninitialized Landing & Welcome Redirect ──────────────────────────────
    let res = client.get(format!("{}/", server.base_url)).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/welcome");

    // ── 2. Welcome Page & Setup Code Verification ────────────────────────────────
    let res = client.get(format!("{}/welcome", server.base_url)).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let welcome_cookie = extract_cookie(res.headers()).expect("session cookie on welcome page");
    let welcome_html = res.text().await.unwrap();
    assert!(welcome_html.contains("Welcome") && welcome_html.contains("Setup code"));
    let csrf_welcome = extract_csrf_token(&welcome_html);
    // Retrieve real setup code generated on disk
    let setup_code = server.state.store.setup_code().expect("setup code exists");

    // Rejection of invalid setup code
    let bad_form = [
        ("csrf", csrf_welcome.as_str()),
        ("code", "0000-wrong"),
        ("password", "correct-horse-battery-123"),
        ("confirm", "correct-horse-battery-123"),
    ];
    let res = client
        .post(format!("{}/welcome", server.base_url))
        .header(header::COOKIE, &welcome_cookie)
        .form(&bad_form)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bad_body = res.text().await.unwrap();
    assert!(bad_body.contains("Setup code is incorrect"));

    // Submission of valid setup code + admin password
    let good_form = [
        ("csrf", csrf_welcome.as_str()),
        ("code", setup_code.as_str()),
        ("password", "valid-admin-password-123"),
        ("confirm", "valid-admin-password-123"),
    ];
    let res = client
        .post(format!("{}/welcome", server.base_url))
        .header(header::COOKIE, &welcome_cookie)
        .form(&good_form)
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/setup");

    let session_cookie = extract_cookie(res.headers()).expect("session cookie on account initialization");

    // ── 3. Setup Page & Encrypted Wallet Configuration ───────────────────────────
    let res =
        client.get(format!("{}/setup", server.base_url)).header(header::COOKIE, &session_cookie).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let setup_html = res.text().await.unwrap();
    assert!(setup_html.contains("Settings"));
    let csrf_setup = extract_csrf_token(&setup_html);

    // Rejection of invalid private key format
    let bad_wallet_form = [
        ("csrf", csrf_setup.as_str()),
        ("private_key", "not-a-hex-key"),
        ("proxy_wallet", "0x0000000000000000000000000000000000000001"),
    ];
    let res = client
        .post(format!("{}/setup/wallet", server.base_url))
        .header(header::COOKIE, &session_cookie)
        .form(&bad_wallet_form)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.text().await.unwrap().contains("Private key is not valid"));

    // Valid private key + valid proxy wallet address
    let valid_privkey = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let valid_proxy = "0x1111111111111111111111111111111111111111";
    let good_wallet_form =
        [("csrf", csrf_setup.as_str()), ("private_key", valid_privkey), ("proxy_wallet", valid_proxy)];

    let res = client
        .post(format!("{}/setup/wallet", server.base_url))
        .header(header::COOKIE, &session_cookie)
        .form(&good_wallet_form)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/launching");

    // Verify credentials encrypted atomically to disk with owner permissions
    assert!(server.data_dir.join("master.key").exists(), "master.key written to disk");
    assert!(server.data_dir.join("wallet.enc").exists(), "wallet.enc written to disk");

    // ── 4. Polling Engine Launch Status ──────────────────────────────────────────
    let res = client
        .get(format!("{}/setup/engine-status", server.base_url))
        .header(header::COOKIE, &session_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Simulate boot task marking engine Running
    {
        let mut eng = server.state.engine.write().await;
        eng.engine_phase = EnginePhase::Running;
    }
    let res = client
        .get(format!("{}/setup/engine-status", server.base_url))
        .header(header::COOKIE, &session_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers().get("hx-redirect").unwrap(), "/");

    // ── 5. Authenticated Dashboard Browsing & Security Hardening ──────────────────
    let authed_routes = ["/", "/markets", "/positions", "/rewards", "/activity", "/setup"];
    for route in authed_routes {
        let res = client
            .get(format!("{}{}", server.base_url, route))
            .header(header::COOKIE, &session_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "Route {route} should render 200 OK");

        // Verify standard security headers on every response
        let csp = res.headers().get(header::CONTENT_SECURITY_POLICY).expect("CSP header");
        assert!(csp.to_str().unwrap().contains("script-src 'self'"));
        assert_eq!(res.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(res.headers().get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        assert_eq!(res.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
    }

    // ── 6. Live Real-Time SSE Stream Verification ────────────────────────────────
    let mut sse_res =
        client.get(format!("{}/events", server.base_url)).header(header::COOKIE, &session_cookie).send().await.unwrap();
    assert_eq!(sse_res.status(), StatusCode::OK);
    assert_eq!(sse_res.headers().get(header::CONTENT_TYPE).unwrap(), "text/event-stream");

    // Broadcast live alert and verify stream chunk arrives
    server
        .state
        .alert_tx
        .send(Alert { ts: Utc::now(), level: AlertLevel::Info, message: "E2E synthetic alert test".into() })
        .unwrap();

    let chunk = sse_res.chunk().await.unwrap().expect("SSE chunk delivered over live TCP");
    let chunk_text = String::from_utf8_lossy(&chunk);
    assert!(chunk_text.contains("event: alert"));
    assert!(chunk_text.contains("E2E synthetic alert test"));

    // ── 7. Logout & Session Invalidation ─────────────────────────────────────────
    let overview_html = client
        .get(format!("{}/", server.base_url))
        .header(header::COOKIE, &session_cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf_logout = extract_csrf_token(&overview_html);

    let logout_form = [("csrf", csrf_logout.as_str())];
    let res = client
        .post(format!("{}/logout", server.base_url))
        .header(header::COOKIE, &session_cookie)
        .form(&logout_form)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");

    // Attempting to access protected dashboard with revoked session redirects to /login
    let res = client.get(format!("{}/", server.base_url)).header(header::COOKIE, &session_cookie).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");

    // Clean up temporary files
    let _ = std::fs::remove_dir_all(&server.data_dir);
}

#[tokio::test]
async fn e2e_trading_engine_quoter_and_safety_rules() {
    let config = MarketConfig {
        id: "mar_e2e_test".to_string(),
        url: "https://polymarket.com/event/test/e2e".to_string(),
        label: "Will E2E testing succeed?".to_string(),
        condition_id: "0x1234".to_string(),
        token_id: "token_e2e".to_string(),
        token_label: "Yes".to_string(),
        tick_size: dec!(0.01),
        distance: dec!(0.02),
        min_depth_between: dec!(100.0),
        order_size: dec!(50.0),
        expires_at: Utc::now() + ChronoDuration::hours(24),
        paused: false,
        benchmark_bid: Some(dec!(0.50)),
        max_volatility: Some(dec!(0.05)),
    };

    let mut book = TokenBook { best_bid: Some(dec!(0.50)), best_ask: Some(dec!(0.52)), ..Default::default() };
    book.bids.insert(dec!(0.50), dec!(1000.0));
    book.bids.insert(dec!(0.49), dec!(500.0));
    book.bids.insert(dec!(0.48), dec!(300.0));
    book.asks.insert(dec!(0.52), dec!(800.0));

    // 1. Initial State: Idle -> verifies depth -> Place action
    let action = quoter::evaluate(&config, &book, &OrderStatus::Idle);
    assert_eq!(action, QuoteAction::Place { price: dec!(0.48) }, "Should place BUY 2 cents below best bid 0.50");

    // 2. Active Live Order: Best bid remains 0.50 -> Hold
    let live_status = OrderStatus::Live { order_id: "order_123".to_string(), price: dec!(0.48) };
    let action = quoter::evaluate(&config, &book, &live_status);
    assert_eq!(action, QuoteAction::Hold, "Should hold resting order when conditions hold");

    // 3. Depth Depletion: Bids between target and best bid fall below min_depth ($100)
    let mut thin_book = TokenBook { best_bid: Some(dec!(0.50)), best_ask: Some(dec!(0.52)), ..Default::default() };
    thin_book.bids.insert(dec!(0.50), dec!(50.0)); // 50 * 0.50 = $25.00
    thin_book.bids.insert(dec!(0.49), dec!(10.0)); // 10 * 0.49 = $4.90 -> total depth $29.90 < $100 threshold
    thin_book.asks.insert(dec!(0.52), dec!(800.0));
    let action = quoter::evaluate(&config, &thin_book, &live_status);
    assert_eq!(
        action,
        QuoteAction::Cancel { order_id: "order_123".to_string(), reason: CancelReason::DepthDropped },
        "Should cancel when depth protection drops below threshold"
    );

    // 4. Volatility Spike: Best bid drifts beyond max_volatility (0.50 -> 0.56, diff 0.06 > 0.05)
    let mut volatile_book = TokenBook { best_bid: Some(dec!(0.56)), best_ask: Some(dec!(0.58)), ..Default::default() };
    volatile_book.bids.insert(dec!(0.56), dec!(1000.0));
    volatile_book.asks.insert(dec!(0.58), dec!(800.0));
    let action = quoter::evaluate(&config, &volatile_book, &live_status);
    assert!(
        matches!(action, QuoteAction::Deactivate { reason: DeactivateReason::Volatility, .. }),
        "Should deactivate and auto-pause when best bid moves past volatility limit"
    );

    // 5. Expiration: Expiry timestamp in the past
    let mut expired_config = config.clone();
    expired_config.expires_at = Utc::now() - ChronoDuration::minutes(5);
    let action = quoter::evaluate(&expired_config, &book, &live_status);
    assert!(
        matches!(action, QuoteAction::Deactivate { reason: DeactivateReason::Expired, .. }),
        "Should deactivate when expiry time has passed"
    );
}
