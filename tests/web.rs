//! Integration tests for the dashboard router — exercised via `oneshot`
//! (auth flow, Askama rendering, rust-embed assets) with no network bind.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use polyfarmer::creds::CredentialStore;
use polyfarmer::web::{router, WebState};
use tower::ServiceExt; // for `oneshot`

fn unique_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // Atomic counter guarantees uniqueness even across parallel tests.
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("pf-web-test-{pid}-{n}"))
}

fn empty_engine() -> Arc<tokio::sync::RwLock<polyfarmer::engine::ws_manager::AppState>> {
    Arc::new(tokio::sync::RwLock::new(
        polyfarmer::engine::ws_manager::AppState::new("markets.json".into()),
    ))
}

/// Router backed by an *initialized* store (password set); returns it + password.
fn test_app() -> (Router, String) {
    let store = CredentialStore::open(unique_dir()).unwrap();
    let pw = "test-password-123".to_string();
    store.set_password(&pw).unwrap();
    (router(WebState::new(Arc::new(store), empty_engine())), pw)
}

/// Router backed by a fresh, *uninitialized* store (first-run state).
fn test_app_uninit() -> Router {
    let store = CredentialStore::open(unique_dir()).unwrap();
    router(WebState::new(Arc::new(store), empty_engine()))
}

async fn body_string(res: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn session_cookie(res: &axum::response::Response) -> Option<String> {
    res.headers()
        .get(header::SET_COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .next()
        .map(|s| s.to_string())
}

fn extract_csrf(html: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = html.find(marker).expect("csrf field") + marker.len();
    let end = html[start..].find('"').unwrap() + start;
    html[start..end].to_string()
}

#[tokio::test]
async fn unauthenticated_dashboard_redirects_to_login() {
    let (app, _) = test_app();
    let res = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(res.status().is_redirection(), "got {}", res.status());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");
}

#[tokio::test]
async fn login_page_renders_with_csrf() {
    let (app, _) = test_app();
    let res = app
        .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res).await;
    assert!(body.contains("polyfarmer"));
    assert!(body.contains("name=\"csrf\""));
}

#[tokio::test]
async fn embedded_assets_are_served() {
    let (app, _) = test_app();
    for path in ["/assets/app.css", "/assets/htmx.min.js"] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
    }
    let res = app
        .oneshot(Request::builder().uri("/assets/nope.txt").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn full_login_grants_access() {
    let (app, pw) = test_app();

    // 1) GET /login → cookie + csrf
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let cookie = session_cookie(&res).expect("session cookie");
    let csrf = extract_csrf(&body_string(res).await);

    // 2) POST /login with correct password → redirect (to /setup, since must_change)
    let form = format!("csrf={csrf}&password={pw}");
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_redirection(), "login should redirect, got {}", res.status());
    // Login rotates the session id (fixation prevention) — follow the new cookie.
    let cookie = session_cookie(&res).unwrap_or(cookie);

    // 3) GET /setup with the authed cookie → 200
    let res = app
        .oneshot(
            Request::builder()
                .uri("/setup")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body_string(res).await.contains("Private key"));
}

#[tokio::test]
async fn non_local_blocked_until_setup() {
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;

    let app = test_app_uninit();

    // Non-loopback peer during first-run → 403.
    let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo("203.0.113.5:5555".parse::<SocketAddr>().unwrap()));
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Loopback peer → allowed through to the wizard.
    let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo("127.0.0.1:5555".parse::<SocketAddr>().unwrap()));
    let res = app.oneshot(req).await.unwrap();
    assert!(res.status().is_redirection());
}

#[tokio::test]
async fn first_run_welcome_creates_account() {
    let app = test_app_uninit();

    // First run: any protected route → /welcome
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(res.status().is_redirection());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/welcome");

    // Welcome page renders with csrf + cookie
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/welcome").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = session_cookie(&res).expect("cookie");
    let csrf = extract_csrf(&body_string(res).await);

    // Create the account → redirect to /setup
    let form = format!("csrf={csrf}&password=supersecret&confirm=supersecret");
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/welcome")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_redirection(), "got {}", res.status());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/setup");
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let (app, _) = test_app();
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let cookie = session_cookie(&res).expect("cookie");
    let csrf = extract_csrf(&body_string(res).await);

    let form = format!("csrf={csrf}&password=definitely-wrong");
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    // Re-renders the login page (200) with an error rather than redirecting.
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body_string(res).await.contains("Incorrect password"));
}

/// Log in against `app` and return the authenticated session cookie.
async fn login(app: &Router, pw: &str) -> String {
    let res = app.clone().oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
    let cookie = session_cookie(&res).expect("session cookie");
    let csrf = extract_csrf(&body_string(res).await);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={csrf}&password={pw}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_redirection());
    session_cookie(&res).unwrap_or(cookie)
}

/// An initialized app whose engine state holds one (paused) market leg.
fn test_app_with_market() -> (Router, String) {
    use polyfarmer::types::{MarketConfig, OrderStatus};
    use rust_decimal_macros::dec;
    let store = CredentialStore::open(unique_dir()).unwrap();
    let pw = "test-password-123".to_string();
    store.set_password(&pw).unwrap();
    let engine = empty_engine();
    {
        let mut s = engine.try_write().unwrap();
        s.configs.push(MarketConfig {
            id: "mar_test_leg".into(),
            url: "https://polymarket.com/event/test-event/test-market".into(),
            label: "Will the test pass?".into(),
            condition_id: "0xabc".into(),
            token_id: "123".into(),
            token_label: "Yes".into(),
            tick_size: dec!(0.01),
            distance: dec!(0.02),
            min_depth_between: dec!(500),
            order_size: dec!(100),
            expires_at: chrono::Utc::now() + chrono::Duration::days(7),
            paused: true,
            benchmark_bid: None,
            max_volatility: None,
        });
        s.order_status.insert("mar_test_leg".into(), OrderStatus::Idle);
    }
    (router(WebState::new(Arc::new(store), engine)), pw)
}

#[tokio::test]
async fn every_page_and_fragment_renders() {
    let (app, pw) = test_app_with_market();
    let cookie = login(&app, &pw).await;
    // None of these need the network: no engine/wallet → account panels
    // render their "not running / no wallet" states.
    for path in [
        "/",
        "/overview/kpis",
        "/overview/book",
        "/overview/fills",
        "/status/strip",
        "/markets",
        "/markets/table",
        "/markets/browse",
        "/markets/mar_test_leg/edit",
        "/positions",
        "/positions/table?tab=open",
        "/positions/table?tab=trades",
        "/rewards",
        "/rewards/table",
        "/activity",
        "/activity/recent",
        "/setup",
    ] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(path).header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        let body = body_string(res).await;
        assert!(!body.contains("template error"), "{path}: {body}");
    }
}

#[tokio::test]
async fn markets_table_shows_min_depth_in_usd_and_real_status() {
    let (app, pw) = test_app_with_market();
    let cookie = login(&app, &pw).await;
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/markets/table").header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_string(res).await;
    assert!(body.contains("Will the test pass?"));
    assert!(body.contains("$500.00"), "min depth shown in USD");
    assert!(body.contains("2.0¢ below bid"), "peg shown");
    assert!(body.contains(">Paused<"), "paused leg");

    // Engine never started → the status strip must not claim it's running.
    let res = app
        .oneshot(Request::builder().uri("/status/strip").header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_string(res).await;
    assert!(body.contains("No wallet"), "{body}");
    assert!(!body.contains(">Running<"));
}

#[tokio::test]
async fn old_reward_history_url_redirects() {
    let (app, pw) = test_app();
    let cookie = login(&app, &pw).await;
    let res = app
        .oneshot(Request::builder().uri("/rewards/history").header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(res.status().is_redirection());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/rewards");
}
