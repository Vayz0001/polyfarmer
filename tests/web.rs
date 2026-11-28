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
    Arc::new(tokio::sync::RwLock::new(polyfarmer::engine::ws_manager::AppState::new("markets.json".into())))
}

/// Router backed by an *initialized* store (password set); returns it + password.
fn test_app() -> (Router, String) {
    let store = CredentialStore::open(unique_dir()).unwrap();
    let pw = "test-password-123".to_string();
    store.set_password(&pw).unwrap();
    (router(WebState::new(Arc::new(store), empty_engine())), pw)
}

async fn body_string(res: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn session_cookie(res: &axum::response::Response) -> Option<String> {
    res.headers().get(header::SET_COOKIE)?.to_str().ok()?.split(';').next().map(|s| s.to_string())
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
    let res = app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
    assert!(res.status().is_redirection(), "got {}", res.status());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");
}

#[tokio::test]
async fn reward_fragments_need_a_login() {
    let (app, _) = test_app();
    for path in ["/rewards", "/rewards/kpis", "/rewards/table", "/rewards/history", "/overview/rewards"] {
        let res = app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert!(res.status().is_redirection(), "{path}: got {}", res.status());
        assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login", "{path}");
    }
}

#[tokio::test]
async fn reward_fragments_without_a_wallet_say_so_and_skip_the_network() {
    let (app, pw) = test_app_with_market();
    let cookie = login(&app, &pw).await;
    let (status, kpis, _) = get_authed(&app, &cookie, "/rewards/kpis").await;
    assert_eq!(status, StatusCode::OK);
    for card in ["Today so far", "Yesterday", "Last 7 days", "All time", "Rewards rank"] {
        assert!(kpis.contains(card), "missing {card}");
    }
    assert!(kpis.contains("engine not running") && kpis.contains("configure your wallet"));
    assert!(kpis.contains("rel=\"noopener noreferrer\""), "external link must not leak the opener");
    let (_, history, _) = get_authed(&app, &cookie, "/rewards/history").await;
    assert!(history.contains("Configure your wallet"));
    let (_, table, _) = get_authed(&app, &cookie, "/rewards/table").await;
    assert!(table.contains("Engine not running"));
}

#[tokio::test]
async fn login_page_renders_with_csrf() {
    let (app, _) = test_app();
    let res = app.oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res).await;
    assert!(body.contains("Polyfarmer"));
    assert!(body.contains("name=\"csrf\""));
    assert!(body.contains("rel=\"icon\" href=\"/assets/favicon.svg\""), "the page links its favicon");
}

#[tokio::test]
async fn embedded_assets_are_served() {
    let (app, _) = test_app();
    for path in ["/assets/app.css", "/assets/htmx.min.js", "/assets/favicon.svg", "/assets/apple-touch-icon.png"] {
        let res = app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
    }
    let res = app.oneshot(Request::builder().uri("/assets/nope.txt").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn full_login_grants_access() {
    let (app, pw) = test_app();

    // 1) GET /login → cookie + csrf
    let res = app.clone().oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
    let cookie = session_cookie(&res).expect("session cookie");
    let csrf = extract_csrf(&body_string(res).await);

    // 2) POST /login with correct password → redirect (to /setup: no wallet yet)
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
        .oneshot(Request::builder().uri("/setup").header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body_string(res).await.contains("Private key"));
}

/// Uninitialised install; returns the router and the pending setup code.
fn test_app_uninit_with_code() -> (Router, String) {
    let store = CredentialStore::open(unique_dir()).unwrap();
    let code = store.setup_code().expect("a fresh install has a setup code");
    (router(WebState::new(Arc::new(store), empty_engine())), code)
}

/// GET /welcome → (session cookie, csrf token, body).
async fn welcome_page(app: &Router, uri: &str) -> (String, String, String) {
    let res = app.clone().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = session_cookie(&res).expect("cookie");
    let body = body_string(res).await;
    let csrf = extract_csrf(&body);
    (cookie, csrf, body)
}

/// POST /welcome with the given code + password.
async fn post_welcome(app: &Router, cookie: &str, csrf: &str, code: &str, pw: &str) -> axum::response::Response {
    let form = format!("csrf={csrf}&code={code}&password={pw}&confirm={pw}");
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/welcome")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn first_run_welcome_creates_account_with_setup_code() {
    let (app, code) = test_app_uninit_with_code();

    // First run: any protected route → /welcome
    let res = app.clone().oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
    assert!(res.status().is_redirection());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/welcome");

    // The page asks for the code…
    let (cookie, csrf, body) = welcome_page(&app, "/welcome").await;
    assert!(body.contains("Setup code") && body.contains("name=\"code\""));

    // …and the correct code creates the account → redirect to /setup.
    let res = post_welcome(&app, &cookie, &csrf, &code, "supersecret-pass-1").await;
    assert!(res.status().is_redirection(), "got {}", res.status());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/setup");
}

#[tokio::test]
async fn setup_is_refused_without_the_right_code_even_from_loopback() {
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;
    let (app, code) = test_app_uninit_with_code();

    // A reverse proxy / Tailscale Serve makes every visitor look like 127.0.0.1
    // — so the address proves nothing; only the code does.
    for peer in ["127.0.0.1:5555", "203.0.113.5:5555"] {
        let mut req = Request::builder().uri("/welcome").body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{peer} can see the form");
    }

    let (cookie, csrf, _) = welcome_page(&app, "/welcome").await;
    for bad in ["", "aaaa-aaaa", "not-the-code"] {
        let res = post_welcome(&app, &cookie, &csrf, bad, "supersecret-pass-1").await;
        assert_eq!(res.status(), StatusCode::OK, "code {bad:?} must not redirect");
        assert!(body_string(res).await.contains("Setup code is incorrect"), "code {bad:?}");
    }
    // …and no account was created: the right code still works afterwards.
    let res = post_welcome(&app, &cookie, &csrf, &code, "supersecret-pass-1").await;
    assert!(res.status().is_redirection());
}

#[tokio::test]
async fn setup_code_link_prefills_and_is_single_use() {
    let (app, code) = test_app_uninit_with_code();
    let (cookie, csrf, body) = welcome_page(&app, &format!("/welcome?code={code}")).await;
    assert!(body.contains(&format!("value=\"{code}\"")), "code from the link is prefilled");

    let res = post_welcome(&app, &cookie, &csrf, &code, "supersecret-pass-1").await;
    assert!(res.status().is_redirection());

    // Setup is over: /welcome bounces to /login and the code can't be reused.
    let res = app.clone().oneshot(Request::builder().uri("/welcome").body(Body::empty()).unwrap()).await.unwrap();
    assert!(res.status().is_redirection());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");
    let res = post_welcome(&app, &cookie, &csrf, &code, "another-long-pass").await;
    assert!(res.status().is_redirection());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");
}

#[tokio::test]
async fn wrong_setup_codes_lock_out_guessing() {
    let (app, code) = test_app_uninit_with_code();
    let (cookie, csrf, _) = welcome_page(&app, "/welcome").await;
    // Test requests carry no peer address, which counts as "proxied" (loopback / a
    // reverse proxy): 15 wrong guesses → locked; even the right code is then refused.
    for _ in 0..15 {
        let res = post_welcome(&app, &cookie, &csrf, "zzzz-zzzz", "supersecret-pass-1").await;
        assert!(body_string(res).await.contains("Setup code is incorrect"));
    }
    let res = post_welcome(&app, &cookie, &csrf, &code, "supersecret-pass-1").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body_string(res).await.contains("Too many attempts"));
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let (app, _) = test_app();
    let res = app.clone().oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
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
        "/overview/rewards",
        "/rewards",
        "/rewards/kpis",
        "/rewards/table",
        "/rewards/history",
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
    assert!(body.contains("2.0¢ below\u{a0}bid"), "peg shown");
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
async fn repeated_wrong_passwords_lock_out_login() {
    let (app, pw) = test_app();
    let res = app.clone().oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
    let cookie = session_cookie(&res).expect("cookie");
    let csrf = extract_csrf(&body_string(res).await);
    let attempt = |password: String| {
        let app = app.clone();
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf}&password={password}")))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    // 15 wrong guesses in a row from a proxied/unknown peer → locked (this limiter
    // used to reset its counter on every check, so it never engaged).
    for _ in 0..15 {
        let res = attempt("definitely-wrong".into()).await;
        assert!(body_string(res).await.contains("Incorrect password"));
    }
    // Now even the CORRECT password is refused until the lockout expires.
    let res = attempt(pw).await;
    assert_eq!(res.status(), StatusCode::OK, "no redirect while locked");
    assert!(body_string(res).await.contains("Too many attempts"));
}

/// The `Set-Cookie` header of GET /login on a router built with `secure`.
async fn login_cookie_header(secure: bool) -> String {
    let store = CredentialStore::open(unique_dir()).unwrap();
    store.set_password("test-password-123").unwrap();
    let app = router(WebState::new(Arc::new(store), empty_engine()).with_secure_cookies(secure));
    let res = app.oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
    res.headers().get(header::SET_COOKIE).expect("session cookie").to_str().unwrap().to_string()
}

#[tokio::test]
async fn session_cookie_is_secure_only_when_enabled() {
    let plain = login_cookie_header(false).await;
    assert!(!plain.contains("Secure"), "plain http must keep working: {plain}");
    assert!(plain.contains("HttpOnly") && plain.contains("SameSite=Lax"));

    let secure = login_cookie_header(true).await;
    assert!(secure.contains("Secure"), "{secure}");
    assert!(secure.contains("HttpOnly") && secure.contains("SameSite=Lax"));
}

// ── Sessions, throttling and password policy ───────────────────────────────────

use polyfarmer::web::session_store::BoundedSessionStore;

fn peer(ip: &str) -> axum::extract::ConnectInfo<std::net::SocketAddr> {
    axum::extract::ConnectInfo(format!("{ip}:5555").parse().unwrap())
}

/// POST /login as `ip` (a direct peer) with `password`, using an already-minted session.
async fn post_login_from(app: &Router, ip: &str, cookie: &str, csrf: &str, password: &str) -> axum::response::Response {
    let mut req = Request::builder()
        .method("POST")
        .uri("/login")
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("csrf={csrf}&password={password}")))
        .unwrap();
    req.extensions_mut().insert(peer(ip));
    app.clone().oneshot(req).await.unwrap()
}

#[tokio::test]
async fn direct_peers_are_throttled_individually_after_five_failures() {
    let (app, pw) = test_app();
    let (cookie, csrf) = {
        let res = app.clone().oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
        (session_cookie(&res).unwrap(), extract_csrf(&body_string(res).await))
    };
    // 5 wrong guesses from 203.0.113.7 → that IP is locked…
    for _ in 0..5 {
        let res = post_login_from(&app, "203.0.113.7", &cookie, &csrf, "definitely-wrong").await;
        assert!(body_string(res).await.contains("Incorrect password"));
    }
    let res = post_login_from(&app, "203.0.113.7", &cookie, &csrf, &pw).await;
    assert_eq!(res.status(), StatusCode::OK, "still locked even with the right password");
    assert!(body_string(res).await.contains("Too many attempts"));

    // …but a different visitor is not affected: the owner can still sign in.
    let res = post_login_from(&app, "203.0.113.99", &cookie, &csrf, &pw).await;
    assert!(res.status().is_redirection(), "other IP logs in fine, got {}", res.status());
}

#[tokio::test]
async fn password_change_signs_out_every_other_session_but_not_this_one() {
    let (app, pw) = test_app();
    let a = login(&app, &pw).await; // the owner's laptop
    let b = login(&app, &pw).await; // a second (possibly stolen) session

    let get = |cookie: String, uri: &'static str| {
        let app = app.clone();
        async move {
            app.oneshot(Request::builder().uri(uri).header(header::COOKIE, cookie).body(Body::empty()).unwrap())
                .await
                .unwrap()
        }
    };
    assert_eq!(get(a.clone(), "/markets").await.status(), StatusCode::OK);
    assert_eq!(get(b.clone(), "/markets").await.status(), StatusCode::OK);

    // Session A changes the password.
    let page = get(a.clone(), "/setup").await;
    let a_cookie = session_cookie(&page).unwrap_or(a.clone());
    let csrf = extract_csrf(&body_string(page).await);
    let form = format!("csrf={csrf}&current={pw}&password=a-brand-new-password&confirm=a-brand-new-password");
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/setup/password")
                .header(header::COOKIE, &a_cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let a_new = session_cookie(&res).unwrap_or(a_cookie);
    assert!(body_string(res).await.contains("Password changed"));

    // The other session is dead (redirected to login); this one keeps working.
    let res = get(b, "/markets").await;
    assert!(res.status().is_redirection(), "stolen/other session must be signed out, got {}", res.status());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");
    assert_eq!(
        get(a_new, "/markets").await.status(),
        StatusCode::OK,
        "the session that changed the password stays signed in"
    );
}

#[tokio::test]
async fn new_passwords_must_meet_the_length_policy() {
    let (app, pw) = test_app();
    let cookie = login(&app, &pw).await;
    let page = app
        .clone()
        .oneshot(Request::builder().uri("/setup").header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let cookie = session_cookie(&page).unwrap_or(cookie);
    let csrf = extract_csrf(&body_string(page).await);
    for (new, expect) in
        [("short", "at least 12"), ("elevenchars", "at least 12"), (&"x".repeat(129)[..], "at most 128")]
    {
        let form = format!("csrf={csrf}&current={pw}&password={new}&confirm={new}");
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/setup/password")
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(body_string(res).await.contains(expect), "{new:?}");
    }
}

#[tokio::test]
async fn anonymous_visitors_cannot_grow_the_session_store_without_bound() {
    let store = CredentialStore::open(unique_dir()).unwrap();
    store.set_password("test-password-123").unwrap();
    let mut state = WebState::new(Arc::new(store), empty_engine());
    state.sessions = BoundedSessionStore::new(40);
    let sessions = state.sessions.clone();
    let app = router(state);
    // 300 cookie-less hits on /login — each mints a CSRF token, i.e. a session record.
    for _ in 0..300 {
        let res = app.clone().oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
    assert!(sessions.len() <= 40, "store grew to {}", sessions.len());
    assert!(!sessions.is_empty());
}

#[tokio::test]
async fn cookie_lifetimes_anonymous_short_logged_in_long() {
    let (app, pw) = test_app();
    let res = app.clone().oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
    let anon = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(anon.contains("Max-Age=1800"), "anonymous = 30 min: {anon}");

    let cookie = session_cookie(&res).unwrap();
    let csrf = extract_csrf(&body_string(res).await);
    let res = app
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
    let authed = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(authed.contains("Max-Age=43200"), "logged-in idle timeout = 12 h: {authed}");
    assert!(authed.contains("HttpOnly") && authed.contains("SameSite=Lax"));
}

#[tokio::test]
async fn secure_deployments_use_the_host_cookie_prefix() {
    let plain = login_cookie_header(false).await;
    assert!(plain.starts_with("polyfarmer_session="), "{plain}");
    let secure = login_cookie_header(true).await;
    assert!(secure.starts_with("__Host-polyfarmer="), "{secure}");
    assert!(secure.contains("Secure") && secure.contains("Path=/") && !secure.contains("Domain="), "{secure}");
}

// ── CSRF hardening and request limits ──────────────────────────────────────────

/// GET `uri` with `cookie`; returns (status, body, new cookie if the session id rotated).
async fn get_authed(app: &Router, cookie: &str, uri: &str) -> (StatusCode, String, Option<String>) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).header(header::COOKIE, cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let new_cookie = session_cookie(&res);
    (status, body_string(res).await, new_cookie)
}

#[tokio::test]
async fn cross_site_writes_are_refused_before_any_handler_runs() {
    let (app, pw) = test_app();
    // A fresh anonymous session (cookie + CSRF token) per attempt — a successful
    // login rotates the session id, so one session can't be reused after it.
    let fresh = || {
        let app = app.clone();
        async move {
            let res = app.oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
            (session_cookie(&res).unwrap(), extract_csrf(&body_string(res).await))
        }
    };
    let post = |site: Option<&'static str>| {
        let (app, pw, fresh) = (app.clone(), pw.clone(), fresh);
        async move {
            let (cookie, csrf) = fresh().await;
            let mut b = Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if let Some(s) = site {
                b = b.header("sec-fetch-site", s);
            }
            app.oneshot(b.body(Body::from(format!("csrf={csrf}&password={pw}"))).unwrap()).await.unwrap()
        }
    };
    // The browser says another site — or another origin on the same site (e.g. a
    // different local app on another port) — sent this: blocked even with a valid token.
    for site in ["cross-site", "same-site"] {
        let res = post(Some(site)).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "Sec-Fetch-Site: {site}");
    }
    // Our own pages (same-origin), typed/bookmarked requests ("none") and
    // header-less clients (curl, tests) are fine.
    for site in [Some("same-origin"), Some("none"), None] {
        assert!(post(site).await.status().is_redirection(), "Sec-Fetch-Site: {site:?} must be allowed");
    }
}

#[tokio::test]
async fn cross_site_get_requests_are_not_blocked() {
    // Only state-changing methods are filtered; following a link into the app works.
    let (app, _) = test_app();
    let res = app
        .oneshot(Request::builder().uri("/login").header("sec-fetch-site", "cross-site").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn logout_requires_the_csrf_token() {
    let (app, pw) = test_app();
    let cookie = login(&app, &pw).await;
    let (status, page, rotated) = get_authed(&app, &cookie, "/markets").await;
    assert_eq!(status, StatusCode::OK);
    let cookie = rotated.unwrap_or(cookie);
    let csrf = extract_csrf(&page);
    assert!(page.contains("action=\"/logout\""), "the sidebar has a logout form with a token");

    let logout = |body: String| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/logout")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    // Without a (valid) token a hostile page can't sign the owner out.
    for body in ["", "csrf=", "csrf=wrong-token"] {
        let res = logout(body.to_string()).await;
        assert!(res.status().is_redirection());
        assert_ne!(res.headers().get(header::LOCATION).unwrap(), "/login", "body {body:?} must not log out");
        assert_eq!(get_authed(&app, &cookie, "/markets").await.0, StatusCode::OK, "still signed in after {body:?}");
    }
    // With the token it signs out.
    let res = logout(format!("csrf={csrf}")).await;
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");
    assert!(get_authed(&app, &cookie, "/markets").await.0.is_redirection(), "session destroyed");
}

#[tokio::test]
async fn preview_endpoint_checks_the_csrf_token() {
    let (app, pw) = test_app();
    let cookie = login(&app, &pw).await;
    let (_, page, rotated) = get_authed(&app, &cookie, "/markets").await;
    let cookie = rotated.unwrap_or(cookie);
    let csrf = extract_csrf(&page);
    let post = |body: String| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/markets/view/preview")
                        .header(header::COOKIE, cookie)
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            body_string(res).await
        }
    };
    // No / wrong token → refused before any network work.
    assert!(post("slug=x&price_cents=18&order_size=100".into()).await.contains("Session expired"));
    assert!(post("csrf=nope&slug=x&price_cents=18&order_size=100".into()).await.contains("Session expired"));
    // A valid token passes the gate (and then fails on the bad price, without network).
    let out = post(format!("csrf={csrf}&slug=x&price_cents=abc&order_size=100")).await;
    assert!(out.contains("Enter a price"), "{out}");
}

#[tokio::test]
async fn oversized_request_bodies_are_rejected() {
    let (app, _) = test_app();
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf=x&password={}", "A".repeat(200 * 1024))))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// ── Security headers, CSP, panic containment, assets ───────────────────────────

use polyfarmer::web::security::{harden, CSP};

fn header_str(res: &axum::response::Response, name: &str) -> Option<String> {
    res.headers().get(name).map(|v| v.to_str().unwrap().to_string())
}

/// Headers every response must carry, whatever produced it.
fn assert_hardened(res: &axum::response::Response, what: &str) {
    assert_eq!(header_str(res, "content-security-policy").as_deref(), Some(CSP), "{what}: CSP");
    assert_eq!(header_str(res, "x-content-type-options").as_deref(), Some("nosniff"), "{what}");
    assert_eq!(header_str(res, "x-frame-options").as_deref(), Some("DENY"), "{what}");
    assert_eq!(header_str(res, "referrer-policy").as_deref(), Some("no-referrer"), "{what}");
    assert_eq!(header_str(res, "cross-origin-opener-policy").as_deref(), Some("same-origin"), "{what}");
    assert_eq!(header_str(res, "cross-origin-resource-policy").as_deref(), Some("same-origin"), "{what}");
    let pp = header_str(res, "permissions-policy").unwrap();
    assert!(pp.contains("camera=()"), "{what}");
    // Chrome logs a console error for every feature name it doesn't know.
    assert!(!pp.contains("bluetooth"), "{what}: unrecognised Permissions-Policy feature");
}

#[test]
fn csp_forbids_inline_script_eval_and_framing() {
    let directive = |name: &str| {
        CSP.split(';')
            .map(str::trim)
            .find(|d| d.starts_with(name))
            .unwrap_or_else(|| panic!("no {name} in CSP"))
            .to_string()
    };
    let script = directive("script-src");
    assert_eq!(script, "script-src 'self'", "scripts only from our own origin: {script}");
    for bad in ["unsafe-eval", "unsafe-inline", "http:", "*"] {
        assert!(!script.contains(bad), "script-src must not allow {bad}");
    }
    assert_eq!(directive("default-src"), "default-src 'self'");
    assert_eq!(directive("frame-ancestors"), "frame-ancestors 'none'");
    assert_eq!(directive("object-src"), "object-src 'none'");
    assert_eq!(directive("base-uri"), "base-uri 'none'");
    assert_eq!(directive("form-action"), "form-action 'self'");
    assert_eq!(directive("connect-src"), "connect-src 'self'");
}

#[tokio::test]
async fn every_kind_of_response_carries_the_security_headers() {
    let (app, pw) = test_app();
    let cookie = login(&app, &pw).await;
    let req = |uri: &str, cookie: Option<&str>| {
        let mut b = Request::builder().uri(uri);
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        b.body(Body::empty()).unwrap()
    };
    let res = app.clone().oneshot(req("/login", None)).await.unwrap();
    assert_hardened(&res, "200 page");
    let res = app.clone().oneshot(req("/", None)).await.unwrap();
    assert!(res.status().is_redirection());
    assert_hardened(&res, "303 redirect");
    let res = app.clone().oneshot(req("/assets/nope.txt", None)).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_hardened(&res, "404");
    let res = app.clone().oneshot(req("/markets", Some(&cookie))).await.unwrap();
    assert_hardened(&res, "authenticated page");
    let res = app.clone().oneshot(req("/markets/table", Some(&cookie))).await.unwrap();
    assert_hardened(&res, "htmx fragment");

    // …including errors produced by the layers themselves (403 cross-site, 413 body cap).
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_hardened(&res, "403");
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("x".repeat(200 * 1024)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_hardened(&res, "413");
}

#[tokio::test]
async fn pages_are_never_cached_but_assets_revalidate_with_an_etag() {
    let (app, pw) = test_app();
    let cookie = login(&app, &pw).await;
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/markets").header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(header_str(&res, "cache-control").as_deref(), Some("no-store"), "account pages must not be cached");

    let res = app.clone().oneshot(Request::builder().uri("/assets/app.js").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header_str(&res, "cache-control").as_deref(), Some("no-cache"));
    assert!(header_str(&res, "content-type").unwrap().contains("javascript"));
    let etag = header_str(&res, "etag").expect("assets carry an ETag");
    assert!(etag.starts_with('"') && etag.len() > 10);

    // A matching validator gets a body-less 304; a stale one gets the file.
    let res = app
        .clone()
        .oneshot(
            Request::builder().uri("/assets/app.js").header(header::IF_NONE_MATCH, &etag).body(Body::empty()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    assert!(body_string(res).await.is_empty());
    let res = app
        .oneshot(
            Request::builder()
                .uri("/assets/app.js")
                .header(header::IF_NONE_MATCH, "\"stale\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn hsts_is_sent_only_when_served_over_https() {
    for (secure, expect) in [(false, false), (true, true)] {
        let store = CredentialStore::open(unique_dir()).unwrap();
        store.set_password("test-password-123").unwrap();
        let app = router(WebState::new(Arc::new(store), empty_engine()).with_secure_cookies(secure));
        let res = app.oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
        let hsts = header_str(&res, "strict-transport-security");
        assert_eq!(hsts.is_some(), expect, "secure={secure}: {hsts:?}");
        if expect {
            assert!(hsts.unwrap().contains("max-age=31536000"));
        }
    }
}

#[tokio::test]
async fn a_handler_panic_becomes_a_generic_500_with_the_headers_and_no_detail() {
    use axum::routing::get;
    async fn boom() -> &'static str {
        panic!("secret internal detail: /home/user/.keys")
    }
    let app = harden(Router::new().route("/ok", get(|| async { "fine" })).route("/boom", get(boom)), false);
    let res = app.clone().oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_hardened(&res, "panic 500");
    let body = body_string(res).await;
    assert!(!body.contains("secret") && !body.contains("/home/user"), "panic detail leaked: {body}");
    // The server keeps serving after a panic.
    let res = app.oneshot(Request::builder().uri("/ok").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(body_string(res).await, "fine");
}

#[tokio::test]
async fn new_scripts_are_served_as_javascript() {
    let (app, _) = test_app();
    for path in ["/assets/theme.js", "/assets/launch.js", "/assets/htmx.min.js"] {
        let res = app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert!(header_str(&res, "content-type").unwrap().contains("javascript"), "{path}");
    }
}

/// The strict CSP only works while templates stay free of inline script and
/// handlers. Fail loudly if one is reintroduced — for ALL templates, including
/// pages no other test renders.
#[test]
fn templates_are_csp_clean() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    let inline_script = regex_lite_find;
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let html = std::fs::read_to_string(&path).unwrap();
        checked += 1;

        // <script> must always have src= (no inline code).
        for tag in html
            .match_indices("<script")
            .map(|(i, _)| &html[i..html[i..].find('>').map(|j| i + j).unwrap_or(html.len())])
        {
            assert!(tag.contains("src="), "{name}: inline <script> would be blocked by the CSP: {tag}");
        }
        // No inline event handlers (onclick=, onload=, …) and no javascript: URLs.
        assert!(!inline_script(&html, " on"), "{name}: inline event-handler attribute");
        assert!(!html.contains("javascript:"), "{name}: javascript: URL");
        // htmx features that need eval: trigger filters `[expr]`, hx-on, js: values.
        assert!(!html.contains("hx-on"), "{name}: hx-on needs eval");
        for tr in
            html.match_indices("hx-trigger=\"").map(|(i, _)| &html[i + 12..i + 12 + html[i + 12..].find('"').unwrap()])
        {
            assert!(!tr.contains('['), "{name}: htmx trigger filter needs eval: {tr}");
        }
        assert!(!html.contains("\"js:") && !html.contains("'js:"), "{name}: htmx js: value needs eval");
    }
    assert!(checked >= 25, "expected to scan the real templates, found {checked}");

    // Base layouts must also switch htmx's own eval / script-tag features off.
    for base in ["base.html", "base_card.html"] {
        let html = std::fs::read_to_string(dir.join(base)).unwrap();
        assert!(
            html.contains("\"allowEval\":false") && html.contains("\"allowScriptTags\":false"),
            "{base}: htmx-config"
        );
        assert!(html.contains("/assets/theme.js"), "{base}: theme loaded from an external file");
    }
}

/// True if `html` contains ` on<letters>=` (an inline handler attribute) — a tiny
/// scan so the test needs no regex dependency.
fn regex_lite_find(html: &str, prefix: &str) -> bool {
    let b = html.as_bytes();
    html.match_indices(prefix).any(|(i, _)| {
        let rest = &b[i + prefix.len()..];
        let n = rest.iter().take_while(|c| c.is_ascii_lowercase()).count();
        n >= 3 && rest.get(n) == Some(&b'=')
    })
}

// ── Hostile input never panics a handler ───────────────────────────────────────

/// Logged-in session cookie + a CSRF token valid for it (taken from a rendered page).
async fn authed_with_csrf(app: &Router, pw: &str) -> (String, String) {
    let cookie = login(app, pw).await;
    let (_, page, rotated) = get_authed(app, &cookie, "/markets").await;
    (rotated.unwrap_or(cookie), extract_csrf(&page))
}

async fn post_form(app: &Router, cookie: &str, uri: &str, body: String) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    (res.status(), body_string(res).await)
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

#[tokio::test]
async fn start_farming_rejects_hostile_values_with_a_message_not_a_panic() {
    let (app, pw) = test_app();
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let start = |price: &str, size: &str, depth: &str, vol: &str, exp: &str| {
        format!(
            "csrf={csrf}&slug=whatever&side=0&sides=one&price_cents={}&order_size={}&min_depth_usd={}&max_volatility_cents={}&expires_in={}",
            enc(price), enc(size), enc(depth), enc(vol), enc(exp)
        )
    };
    let cases = [
        // (price, size, depth, volatility, expiry) -> text the error must contain
        (start("18", "100", "0", "", "7日"), "Invalid expiry"), // multibyte: used to panic
        (start("18", "100", "0", "", "99999999999999d"), "Invalid expiry"), // duration overflow: used to panic
        (start("18", "100", "0", "", "99999999999999999999d"), "Invalid expiry"),
        (start("18", "100", "0", "", "💥"), "Invalid expiry"),
        (start("18", "79228162514264337593543950335", "0", "", "7d"), "Order size"), // Decimal-max size
        (start("0.0000000000000000000000000001", "100", "0", "", "7d"), "price between"), // size/price overflow
        (start("-5", "100", "0", "", "7d"), "price between"),
        (start("18", "100", "99999999999999999999", "", "7d"), "Depth ahead"),
        (start("18", "100", "0", "1e999", "7d"), "Auto-pause"),
        (start("18", "100", "0", "101", "7d"), "Auto-pause"),
        (start("abc", "100", "0", "", "7d"), "price between"),
    ];
    for (body, expect) in cases {
        let (status, out) = post_form(&app, &cookie, "/markets/start", body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(out.contains(expect), "expected {expect:?} in response for {body}\n got: {out}");
        assert!(!out.contains("Internal error"), "handler panicked for {body}");
    }
}

#[tokio::test]
async fn edit_rejects_hostile_values_and_leaves_the_config_untouched() {
    let (app, pw) = test_app_with_market();
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let edit = |size: &str, dist: &str, depth: &str, vol: &str, exp: &str| {
        format!(
            "csrf={csrf}&order_size={}&distance_cents={}&min_depth_usd={}&max_volatility_cents={}&expires_in={}",
            enc(size),
            enc(dist),
            enc(depth),
            enc(vol),
            enc(exp)
        )
    };
    for (body, expect) in [
        (edit("100", "2", "500", "", "7日"), "Invalid expiry"),
        (edit("100", "2", "500", "", "99999999999999d"), "Invalid expiry"),
        (edit("79228162514264337593543950335", "2", "500", "", "keep"), "Order size"),
        (edit("100", "9999", "500", "", "keep"), "Distance"),
        (edit("100", "2", "-1", "", "keep"), "Depth ahead"),
        (edit("100", "2", "500", "5000", "keep"), "Auto-pause"),
    ] {
        let (status, out) = post_form(&app, &cookie, "/markets/mar_test_leg/edit", body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(out.contains(expect), "expected {expect:?} for {body}\n got: {out}");
        assert!(!out.contains("Internal error"), "panicked for {body}");
    }
    // None of the rejected edits changed the saved leg (still $100, 2¢, $500 depth).
    let (_, table, _) = get_authed(&app, &cookie, "/markets/table").await;
    assert!(table.contains("$100") && table.contains("$500.00") && table.contains("2.0¢ below\u{a0}bid"), "{table}");
}

#[tokio::test]
async fn oversized_free_text_and_odd_parameters_are_bounded() {
    let (app, pw) = test_app();
    let cookie = login(&app, &pw).await;
    // A 5,000-character search is cut to 200 characters before it reaches the page.
    let (status, page, _) = get_authed(&app, &cookie, &format!("/markets/browse?q={}", "a".repeat(5000))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(&"a".repeat(200)) && !page.contains(&"a".repeat(201)), "search text must be truncated");
    // An absurd slug is refused without being sent upstream.
    let (status, page, _) = get_authed(&app, &cookie, &format!("/markets/view?slug={}", "x".repeat(300))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("look like a Polymarket market link"), "{page}");
    // An unknown sort column falls back to the default rather than going upstream.
    let (status, page, _) = get_authed(&app, &cookie, "/markets/browse?sort=%27%3B%20DROP%20TABLE&dir=sideways").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("value=\"rate_per_day\" selected") && page.contains("value=\"DESC\" selected"),
        "defaults applied"
    );
}

#[tokio::test]
async fn a_failed_engine_start_hints_at_network_vpn_and_region_causes() {
    use polyfarmer::types::EnginePhase;
    let store = CredentialStore::open(unique_dir()).unwrap();
    store.set_password("test-password-123").unwrap();
    store.set_wallet(&format!("0x{}", "11".repeat(32)), "0x0000000000000000000000000000000000000001").unwrap();
    let engine = empty_engine();
    engine.try_write().unwrap().engine_phase = EnginePhase::Error;
    let app = router(WebState::new(Arc::new(store), engine));
    let cookie = login(&app, "test-password-123").await;

    let (_, overview, _) = get_authed(&app, &cookie, "/").await;
    assert!(
        overview.contains("Start failed") && overview.contains("VPN") && overview.contains("restricts some regions")
    );
    let (_, launch, _) = get_authed(&app, &cookie, "/setup/engine-status").await;
    assert!(launch.contains("VPN") && launch.contains("restricts some regions"), "launch screen hint");
}

/// One leg in `status` with its markets.json in a temp dir; returns the
/// router, password, engine state and that file.
fn app_with_leg(
    status: polyfarmer::types::OrderStatus,
) -> (Router, String, Arc<tokio::sync::RwLock<polyfarmer::engine::ws_manager::AppState>>, std::path::PathBuf) {
    let (app, pw, engine, markets_file, _) = app_state_with_leg(status);
    (app, pw, engine, markets_file)
}

/// [`app_with_leg`] plus its web state, to seed its caches.
fn app_state_with_leg(
    status: polyfarmer::types::OrderStatus,
) -> (Router, String, Engine, std::path::PathBuf, WebState) {
    use polyfarmer::types::MarketConfig;
    use rust_decimal_macros::dec;
    let dir = unique_dir();
    let store = CredentialStore::open(&dir).unwrap();
    let pw = "test-password-123".to_string();
    store.set_password(&pw).unwrap();
    let markets_file = dir.join("markets.json");
    let engine =
        Arc::new(tokio::sync::RwLock::new(polyfarmer::engine::ws_manager::AppState::new(markets_file.clone())));
    {
        let mut s = engine.try_write().unwrap();
        s.configs.push(MarketConfig {
            id: "mar_leg".into(),
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
            paused: false,
            benchmark_bid: None,
            max_volatility: None,
        });
        s.order_status.insert("mar_leg".into(), status);
    }
    let state = WebState::new(Arc::new(store), Arc::clone(&engine));
    (router(state.clone()), pw, engine, markets_file, state)
}

/// What an edit of size or distance checks, offline: the engine's live book
/// for `token` (best bid `best_bid`) and Polymarket's minimum order (shares)
/// in the cache, so the edit never reads it from Polymarket.
async fn seed_book_and_min(
    state: &WebState,
    token: &str,
    best_bid: rust_decimal::Decimal,
    min_shares: rust_decimal::Decimal,
) {
    let book = polyfarmer::engine::orderbook::TokenBook { best_bid: Some(best_bid), ..Default::default() };
    state.engine.write().await.books.insert(token.into(), book);
    state.caches.min_order_size.put(token, min_shares);
}

/// Body and the `HX-Trigger` header of a remove.
async fn post_remove(app: &Router, pw: &str) -> (String, Option<String>) {
    let cookie = login(app, pw).await;
    let (_, page, rotated) = get_authed(app, &cookie, "/markets").await;
    let cookie = rotated.unwrap_or(cookie);
    let csrf = extract_csrf(&page);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/markets/mar_leg/remove")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={csrf}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let trigger = res.headers().get("HX-Trigger").map(|v| v.to_str().unwrap().to_string());
    (body_string(res).await, trigger)
}

#[tokio::test]
async fn removing_a_leg_without_an_order_removes_it_in_one_go() {
    let (app, pw, engine, markets_file) = app_with_leg(polyfarmer::types::OrderStatus::Idle);
    let (body, trigger) = post_remove(&app, &pw).await;
    assert!(!body.contains("Will the test pass?"), "{body}");
    assert_eq!(trigger, None, "no error toast on success");
    let s = engine.read().await;
    assert!(s.configs.is_empty() && !s.order_status.contains_key("mar_leg"));
    let saved = std::fs::read_to_string(markets_file).unwrap();
    assert!(!saved.contains("mar_leg"), "{saved}");
}

#[tokio::test]
async fn a_leg_whose_order_cannot_be_cancelled_is_kept_paused_not_removed() {
    // A live order and no engine to cancel it with: removing must not drop the
    // leg, or the order would sit on Polymarket with nothing tracking it.
    let status =
        polyfarmer::types::OrderStatus::Live { order_id: "0xorder".into(), price: rust_decimal_macros::dec!(0.4) };
    let (app, pw, engine, markets_file) = app_with_leg(status);
    let (body, trigger) = post_remove(&app, &pw).await;
    assert!(body.contains("cancel its order on Polymarket") && body.contains("try removing it again"), "{body}");
    // Also sent as a toast, which the panel's next refresh can't wipe.
    let trigger = trigger.expect("HX-Trigger header");
    let event: serde_json::Value = serde_json::from_str(&trigger).unwrap();
    assert!(event["pf-error"].as_str().unwrap().contains("try removing it again"), "{trigger}");
    assert!(body.contains("Will the test pass?"), "leg still listed");
    let s = engine.read().await;
    assert_eq!(s.configs.len(), 1);
    assert!(s.configs[0].paused, "kept, and paused");
    assert!(
        matches!(s.order_status.get("mar_leg"), Some(polyfarmer::types::OrderStatus::Live { .. })),
        "still tracked"
    );
    let saved = std::fs::read_to_string(markets_file).unwrap();
    assert!(saved.contains("\"paused\": true"), "{saved}");
}

/// Resume after an auto-pause restarts the limit from today's best bid, so
/// the leg quotes again instead of sitting over its old limit. A leg that
/// wasn't paused, or has no limit, keeps its reference.
#[tokio::test]
async fn resume_restarts_auto_pause_from_the_current_best_bid() {
    use polyfarmer::engine::orderbook::TokenBook;
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    let (app, pw, engine, markets_file) = app_with_leg(OrderStatus::Idle);
    {
        let mut s = engine.write().await;
        let leg = &mut s.configs[0];
        leg.paused = true; // auto-paused at a 5¢ limit from 20¢
        leg.max_volatility = Some(dec!(0.05));
        leg.benchmark_bid = Some(dec!(0.20));
        let mut other = leg.clone(); // the other outcome, running, same limit
        other.id = "mar_other".into();
        other.token_id = "456".into();
        other.paused = false;
        s.configs.push(other);
        s.books.insert(
            "123".into(),
            TokenBook { best_bid: Some(dec!(0.40)), best_ask: Some(dec!(0.41)), ..Default::default() },
        );
        s.books.insert(
            "456".into(),
            TokenBook { best_bid: Some(dec!(0.59)), best_ask: Some(dec!(0.60)), ..Default::default() },
        );
    }
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/markets/group/0xabc/resume")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={csrf}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let s = engine.read().await;
    let leg = s.configs.iter().find(|c| c.id == "mar_leg").unwrap();
    assert!(!leg.paused);
    assert_eq!(leg.benchmark_bid, Some(dec!(0.40)), "restarted from the current best bid");
    let other = s.configs.iter().find(|c| c.id == "mar_other").unwrap();
    assert_eq!(other.benchmark_bid, Some(dec!(0.20)), "a running leg keeps its reference");
    let saved = std::fs::read_to_string(&markets_file).unwrap();
    let saved: Vec<polyfarmer::types::MarketConfig> = serde_json::from_str(&saved).unwrap();
    let saved_leg = saved.iter().find(|c| c.id == "mar_leg").unwrap();
    assert_eq!((saved_leg.paused, saved_leg.benchmark_bid), (false, Some(dec!(0.40))), "saved");
    drop(s);

    // The quoter no longer sees the old move: the resumed leg is free to quote.
    let s = engine.read().await;
    let book = s.books.get("123").unwrap();
    let action = polyfarmer::engine::quoter::evaluate(leg_of(&s, "mar_leg"), book, &OrderStatus::Idle, &[]);
    assert!(!matches!(action, polyfarmer::engine::quoter::QuoteAction::Deactivate { .. }), "not re-paused: {action:?}");
}

fn leg_of<'a>(s: &'a polyfarmer::engine::ws_manager::AppState, id: &str) -> &'a polyfarmer::types::MarketConfig {
    s.configs.iter().find(|c| c.id == id).unwrap()
}

/// If saving fails, a resume is undone exactly: the paused leg stays paused
/// with its old reference, and a leg that was already running stays running.
#[tokio::test]
async fn a_failed_save_undoes_a_resume_exactly() {
    use polyfarmer::engine::orderbook::TokenBook;
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    let (app, pw, engine, markets_file) = app_with_leg(OrderStatus::Idle);
    {
        let mut s = engine.write().await;
        let leg = &mut s.configs[0];
        leg.paused = true;
        leg.max_volatility = Some(dec!(0.05));
        leg.benchmark_bid = Some(dec!(0.20));
        let mut other = leg.clone();
        other.id = "mar_other".into();
        other.token_id = "456".into();
        other.paused = false;
        s.configs.push(other);
        s.books.insert(
            "123".into(),
            TokenBook { best_bid: Some(dec!(0.40)), best_ask: Some(dec!(0.41)), ..Default::default() },
        );
        // A directory where the file should be: every save fails.
        std::fs::create_dir_all(markets_file.with_extension("dir")).unwrap();
        s.markets_file = markets_file.with_extension("dir");
    }
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    for uri in ["/markets/mar_leg/resume", "/markets/mar_other/resume"] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::COOKIE, cookie.clone())
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_string(res).await;
        assert!(body.contains("Failed to save"), "{uri}: {body}");
    }
    let s = engine.read().await;
    let leg = leg_of(&s, "mar_leg");
    assert_eq!((leg.paused, leg.benchmark_bid), (true, Some(dec!(0.20))), "resume undone");
    let other = leg_of(&s, "mar_other");
    assert_eq!((other.paused, other.benchmark_bid), (false, Some(dec!(0.20))), "running leg left running");
}

/// The safeguards use one set of names: Depth ahead, Auto-pause, Expires.
#[tokio::test]
async fn safeguards_use_the_same_names_everywhere() {
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    let (app, pw, engine, _) = app_with_leg(OrderStatus::Idle);
    {
        let mut s = engine.write().await;
        s.configs[0].max_volatility = Some(dec!(0.05));
        s.configs[0].benchmark_bid = Some(dec!(0.31));
    }
    let cookie = login(&app, &pw).await;
    let (_, table, _) = get_authed(&app, &cookie, "/markets/table").await;
    assert!(table.contains(">Depth ahead</th>") && table.contains(">Distance</th>"), "{table}");
    assert!(table.contains(r#"title="Auto-pauses if the best bid reaches 26.0¢ or 36.0¢">±5.0¢"#), "{table}");
    let (_, drawer, _) = get_authed(&app, &cookie, "/markets/mar_leg/edit").await;
    for name in ["Depth ahead", "Auto-pause", "Expires", ">Size<", "Distance"] {
        assert!(drawer.contains(name), "drawer lacks {name}");
    }
    for old in ["Min depth", "Stop quoting", "if best bid moves", "Order size", "Custom… from now"] {
        assert!(!drawer.contains(old), "drawer still says {old}");
    }
    assert!(drawer.contains("left</span>"), "time left shown");
}

/// Settings → Defaults for new markets: shows the built-in values, saves
/// valid ones (and refuses others, keeping what was typed), resets; the
/// new-order form's "Save as my defaults" saves its size and safeguards.
#[tokio::test]
async fn defaults_for_new_markets_save_and_reset() {
    let dir = unique_dir();
    let store = CredentialStore::open(&dir).unwrap();
    let pw = "test-password-123".to_string();
    store.set_password(&pw).unwrap();
    let app = router(WebState::new(Arc::new(store), empty_engine()));
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let post = |uri: &'static str, body: String, htmx: bool| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            let mut req = Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if htmx {
                req = req.header("HX-Request", "true");
            }
            let res = app.oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
            (res.status(), body_string(res).await)
        }
    };

    let (_, page, _) = get_authed(&app, &cookie, "/setup").await;
    assert!(page.contains("Defaults for new markets") && page.contains("built-in values"), "{page}");
    assert!(page.contains(r#"id="d-size" value="100""#) && page.contains(r#"id="d-depth" value="500""#));
    assert!(page.contains(r#"id="d-vol" value="5""#) && page.contains(r#"<option value="7d" selected>"#));
    assert!(!page.contains("Reset to built-in"), "nothing to reset yet");

    // Save from Settings.
    let form = |size: &str, vol: &str| {
        format!(
            "csrf={csrf}&order_size={size}&min_depth_usd=250&max_volatility_cents={vol}&expires_in=custom&expires_custom=12&expires_custom_unit=h"
        )
    };
    let (status, panel) = post("/setup/defaults", form("40", ""), true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(panel.contains("Saved.") && panel.contains("your values"), "{panel}");
    assert!(panel.contains(r#"id="d-size" value="40""#) && panel.contains(r#"id="d-vol" value="""#), "{panel}");
    assert!(panel.contains(r#"<option value="custom" selected>"#) && panel.contains(r#"value="12""#));
    assert!(panel.contains("Reset to built-in"));
    let saved = std::fs::read_to_string(dir.join("defaults.json")).unwrap();
    assert!(saved.contains("\"order_size\": \"40\"") && saved.contains("\"auto_pause_cents\": null"), "{saved}");

    // Refused: an error, what was typed kept, the file untouched.
    let (_, panel) = post("/setup/defaults", form("0", "3"), true).await;
    assert!(panel.contains("alert err") && panel.contains("Order size"), "{panel}");
    assert!(panel.contains(r#"id="d-vol" value="3""#), "typed value kept");
    assert_eq!(std::fs::read_to_string(dir.join("defaults.json")).unwrap(), saved);

    // A bad CSRF token changes nothing; a non-htmx post just goes back to Settings.
    let (_, panel) = post("/setup/defaults", form("77", "").replace(&csrf, "wrong"), true).await;
    assert!(panel.contains("Invalid session"));
    let (status, _) = post("/setup/defaults", form("77", ""), false).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(std::fs::read_to_string(dir.join("defaults.json")).unwrap(), saved);

    // From the new-order form (its other fields are ignored).
    let order = format!(
        "csrf={csrf}&slug=x&side=0&sides=both&price_cents=12&order_size=150&min_depth_usd=1000&max_volatility_cents=2.5&expires_in=3d"
    );
    let (_, note) = post("/setup/defaults/from-order", order, true).await;
    assert!(note.contains("Saved as your defaults"), "{note}");
    let (_, page, _) = get_authed(&app, &cookie, "/setup").await;
    assert!(page.contains(r#"id="d-size" value="150""#) && page.contains(r#"id="d-depth" value="1000""#));
    assert!(page.contains(r#"id="d-vol" value="2.5""#) && page.contains(r#"<option value="3d" selected>"#));
    let (_, note) = post("/setup/defaults/from-order", format!("csrf={csrf}&order_size=abc&expires_in=7d"), true).await;
    assert!(note.contains(r#"class="err""#), "{note}");

    // Bad CSRF on the other two routes changes nothing either.
    let before = std::fs::read_to_string(dir.join("defaults.json")).unwrap();
    let (_, note) = post("/setup/defaults/from-order", "csrf=wrong&order_size=9&expires_in=7d".into(), true).await;
    assert!(note.contains("Invalid session"), "{note}");
    let (_, panel) = post("/setup/defaults/reset", "csrf=wrong".into(), true).await;
    assert!(panel.contains("Invalid session"), "{panel}");
    assert_eq!(std::fs::read_to_string(dir.join("defaults.json")).unwrap(), before);
    // A size the form's input would refuse (step 0.01) isn't saved.
    let (_, note) =
        post("/setup/defaults/from-order", format!("csrf={csrf}&order_size=37.555&expires_in=7d"), true).await;
    assert!(note.contains("whole cents"), "{note}");
    // A size worked out from shares (108 × 37¢) keeps its cents.
    let (_, note) =
        post("/setup/defaults/from-order", format!("csrf={csrf}&order_size=39.96&expires_in=7d"), true).await;
    assert!(note.contains("Saved as your defaults"), "{note}");
    assert!(std::fs::read_to_string(dir.join("defaults.json")).unwrap().contains("39.96"));

    // Reset.
    let (_, panel) = post("/setup/defaults/reset", format!("csrf={csrf}"), true).await;
    assert!(panel.contains("Back to the built-in values") && panel.contains(r#"id="d-size" value="100""#));
    assert!(!dir.join("defaults.json").exists());
}

/// Resuming a leg with no auto-pause limit needs no price and changes nothing else.
#[tokio::test]
async fn resume_without_a_limit_leaves_the_reference_alone() {
    use polyfarmer::types::OrderStatus;
    let (app, pw, engine, _) = app_with_leg(OrderStatus::Idle);
    engine.write().await.configs[0].paused = true;
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/markets/mar_leg/resume")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={csrf}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let s = engine.read().await;
    assert!(!s.configs[0].paused);
    assert_eq!(s.configs[0].benchmark_bid, None);
}

/// "Expires → Custom…": whole hours or days, 1 hour to 1 year, on
/// both the start form and the edit drawer; a valid custom edit is saved.
#[tokio::test]
async fn custom_expiry_is_validated_and_saved() {
    use polyfarmer::types::{MarketConfig, OrderStatus};
    use rust_decimal_macros::dec;
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let markets_file = dir.join("markets.json");
    let store = CredentialStore::open(dir.join("data")).unwrap();
    let pw = "test-password-123".to_string();
    store.set_password(&pw).unwrap();
    let engine =
        Arc::new(tokio::sync::RwLock::new(polyfarmer::engine::ws_manager::AppState::new(markets_file.clone())));
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
    let app = router(WebState::new(Arc::new(store), engine.clone()));
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;

    // Both forms offer the Custom option and the hours/days inputs.
    let (_, drawer, _) = get_authed(&app, &cookie, "/markets/mar_test_leg/edit").await;
    assert!(drawer.contains(r#"value="custom""#) && drawer.contains(r#"name="expires_custom""#), "{drawer}");
    assert!(drawer.contains(r#"<option value="h" selected>hours</option>"#));

    let edit = |exp: &str, n: &str, unit: &str| {
        format!(
            "csrf={csrf}&order_size=100&distance_cents=2&min_depth_usd=500&max_volatility_cents=&expires_in={}&expires_custom={}&expires_custom_unit={}",
            enc(exp),
            enc(n),
            enc(unit)
        )
    };
    let start = |n: &str, unit: &str| {
        format!(
            "csrf={csrf}&slug=whatever&side=0&sides=one&price_cents=18&order_size=100&min_depth_usd=0&max_volatility_cents=&expires_in=custom&expires_custom={}&expires_custom_unit={}",
            enc(n),
            enc(unit)
        )
    };
    for (uri, body, expect) in [
        ("/markets/mar_test_leg/edit", edit("custom", "", "h"), "at least 1 hour"),
        ("/markets/mar_test_leg/edit", edit("custom", "0", "h"), "between 1 hour and 1 year"),
        ("/markets/mar_test_leg/edit", edit("custom", "367", "d"), "between 1 hour and 1 year"),
        ("/markets/mar_test_leg/edit", edit("custom", "8785", "h"), "between 1 hour and 1 year"),
        ("/markets/mar_test_leg/edit", edit("custom", "1.5", "h"), "Invalid expiry"),
        ("/markets/mar_test_leg/edit", edit("custom", "12", "m"), "Invalid expiry unit"),
        ("/markets/mar_test_leg/edit", edit("30m", "", ""), "between 1 hour and 1 year"),
        ("/markets/start", start("", "h"), "at least 1 hour"),
        ("/markets/start", start("0", "d"), "between 1 hour and 1 year"),
        ("/markets/start", start("12", "weeks"), "Invalid expiry unit"),
        ("/markets/start", start("💥", "h"), "Invalid expiry"),
    ] {
        let (status, out) = post_form(&app, &cookie, uri, body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(out.contains(expect), "expected {expect:?} for {body}\n got: {out}");
        assert!(!out.contains("Internal error"), "panicked for {body}");
    }
    let before = engine.read().await.configs[0].expires_at;
    assert!(before > chrono::Utc::now() + chrono::Duration::days(6), "rejected edits left the expiry alone");
    assert!(!markets_file.exists(), "rejected edits saved nothing");

    // A valid custom edit (12 hours from now) lands in memory and on disk.
    let (status, out) = post_form(&app, &cookie, "/markets/mar_test_leg/edit", edit("custom", "12", "h")).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let left = engine.read().await.configs[0].expires_at - chrono::Utc::now();
    assert!((left.num_seconds() - 12 * 3600).abs() < 30, "expires in ~12h, got {left:?}");
    let saved = std::fs::read_to_string(&markets_file).unwrap();
    let saved_at = engine.read().await.configs[0].expires_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    assert!(saved.contains(&saved_at[..16]), "saved file has the new expiry: {saved}");
    // The 1-hour minimum is accepted too.
    let (_, out) = post_form(&app, &cookie, "/markets/mar_test_leg/edit", edit("custom", "1", "h")).await;
    let left = engine.read().await.configs[0].expires_at - chrono::Utc::now();
    assert!((left.num_seconds() - 3600).abs() < 30, "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rejected edit re-opens the drawer with what the user typed (escaped and
/// length-capped), not the saved values, so nothing has to be re-entered.
#[tokio::test]
async fn a_rejected_edit_keeps_what_was_typed() {
    let (app, pw) = test_app_with_market();
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    // Valid size/distance/depth typed, but a custom expiry of 400 days (over a year).
    let body = format!(
        "csrf={csrf}&order_size=250&distance_cents=3.5&min_depth_usd=900&max_volatility_cents=4&expires_in=custom&expires_custom=400&expires_custom_unit=d"
    );
    let (status, out) = post_form(&app, &cookie, "/markets/mar_test_leg/edit", body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(out.contains("between 1 hour and 1 year"), "{out}");
    for typed in [r#"value="250""#, r#"value="3.5""#, r#"value="900""#, r#"value="4""#, r#"value="400""#] {
        assert!(out.contains(typed), "missing {typed}");
    }
    assert!(out.contains(r#"<option value="custom" selected>"#) && out.contains(r#"<option value="d" selected>days"#));
    assert!(!out.contains(r#"<option value="keep" selected>"#));

    // Hostile or oversized input is escaped and cut, and an unknown choice falls back to "keep".
    let evil = enc(r#""><script>alert(1)</script>"#);
    let long = "9".repeat(5000);
    let body = format!(
        "csrf={csrf}&order_size={evil}&distance_cents={long}&min_depth_usd=500&max_volatility_cents=&expires_in=%22%3E%3Cb%3E&expires_custom=&expires_custom_unit=x"
    );
    let (_, out) = post_form(&app, &cookie, "/markets/mar_test_leg/edit", body).await;
    assert!(out.contains("alert err"), "rejected: {out}");
    assert!(!out.contains("<script>alert(1)</script>") && !out.contains("<b>"), "echo is escaped");
    assert!(
        out.contains(&format!(r#"value="{}""#, "9".repeat(64))) && !out.contains(&"9".repeat(65)),
        "echo is capped"
    );
    assert!(out.contains(r#"<option value="keep" selected>"#) && out.contains(r#"<option value="h" selected>hours"#));

    // Nothing was saved by either rejected edit.
    let (_, table, _) = get_authed(&app, &cookie, "/markets/table").await;
    assert!(table.contains("$100") && table.contains("$500.00") && table.contains("2.0¢ below\u{a0}bid"), "{table}");
}

/// Markup the dashboard's keyboard and refresh behaviour relies on: stable
/// ids on every row control (htmx restores focus by id after the 5s
/// refresh), the dialog's action label, the pinned actions column, one name
/// for the market finder, and a paste box on the link-error page.
#[tokio::test]
async fn markets_markup_supports_focus_restore_and_clear_names() {
    let (app, pw) = test_app_with_market();
    let cookie = login(&app, &pw).await;

    let (status, table, _) = get_authed(&app, &cookie, "/markets/table").await;
    assert_eq!(status, StatusCode::OK);
    for id in ["leg-mar_test_leg-toggle", "leg-mar_test_leg-edit", "leg-mar_test_leg-remove", "markets-score-tip"] {
        assert_eq!(table.matches(&format!("id=\"{id}\"")).count(), 1, "exactly one #{id}");
    }
    assert!(table.contains("id=\"mkt-0xabc-name\"") && table.contains("id=\"mkt-0xabc-ext\""));
    assert!(table.contains("data-confirm-label=\"Remove\""), "the dialog names the action");
    assert_eq!(table.matches("class=\"actions-col\"").count(), 2, "header + one row");
    assert!(
        table.contains("title=\"Paused: no order. Resume to quote again. If the bot paused it, Activity says why.\""),
        "status explained on hover"
    );

    let (_, page, _) = get_authed(&app, &cookie, "/markets").await;
    assert!(page.contains("Find markets") && page.contains(">Open link</button>"));
    assert!(!page.contains("Add market"));
    assert!(page.contains("id=\"markets-panel\" tabindex=\"-1\""), "focus can wait on the panel after a Remove");

    let (_, browse, _) = get_authed(&app, &cookie, "/markets/browse").await;
    assert!(browse.contains("<h1>Find markets</h1>"));

    // A link that can't be opened leads back to a paste box, not a dead end.
    let (status, err, _) = get_authed(&app, &cookie, &format!("/markets/view?slug={}", "x".repeat(300))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(err.contains("action=\"/markets/resolve\"") && err.contains("name=\"url\""), "{err}");

    // The drawer starts inert (no Tab stops or Enter-submit while closed).
    assert!(page.contains("<aside id=\"pf-drawer\" class=\"drawer\" aria-hidden=\"true\" inert></aside>"));
}

/// POST a form as a logged-in user: body and `HX-Trigger`.
async fn post_as(app: &Router, cookie: &str, uri: &str, body: String) -> (String, Option<String>) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let trigger = res.headers().get("HX-Trigger").map(|v| v.to_str().unwrap().to_string());
    (body_string(res).await, trigger)
}

/// The market page's "You're farming this" panel has the Markets table's
/// statuses and actions, shows without a running engine, and each action
/// answers with the panel when sent from it (and the table otherwise).
#[tokio::test]
async fn the_market_page_panel_pauses_resumes_and_removes_legs() {
    use polyfarmer::types::OrderStatus;
    let (app, pw, engine, markets_file) = app_with_leg(OrderStatus::Idle);
    let cookie = login(&app, &pw).await;
    let (_, page, rotated) = get_authed(&app, &cookie, "/markets").await;
    let cookie = rotated.unwrap_or(cookie);
    let csrf = extract_csrf(&page);

    let (status, panel, _) = get_authed(&app, &cookie, "/markets/view/position?cid=0xabc").await;
    assert_eq!(status, StatusCode::OK);
    assert!(panel.contains("You're farming this") && panel.contains(r#"data-farmed="123""#), "{panel}");
    assert!(panel.contains(">Waiting<") && panel.contains("$100.00"), "same status and size as Markets: {panel}");
    for id in ["pos-mar_leg-toggle", "pos-mar_leg-edit", "pos-mar_leg-remove"] {
        assert!(panel.contains(&format!(r#"id="{id}""#)), "{id}: {panel}");
    }
    assert!(panel.contains(r#"&#34;view&#34;:&#34;0xabc&#34;"#) && panel.contains(r##"hx-target="#your-position""##));
    assert!(panel.contains(r#"data-confirm-label="Remove""#) && !panel.contains("pos-mar_leg-extend"));
    let (_, other, _) = get_authed(&app, &cookie, "/markets/view/position?cid=0xother").await;
    assert!(!other.contains("You're farming this"), "not farmed here: nothing shown");

    // Pause from the panel: the panel comes back, now offering Resume.
    let (body, _) = post_as(&app, &cookie, "/markets/mar_leg/pause", format!("csrf={csrf}&view=0xabc")).await;
    assert!(body.contains("You're farming this") && body.contains(r#"title="Resume""#) && !body.contains("<table"));
    assert!(engine.read().await.configs[0].paused);
    assert!(std::fs::read_to_string(&markets_file).unwrap().contains(r#""paused": true"#));
    // The same action from the Markets page still answers with the table.
    let (body, _) = post_as(&app, &cookie, "/markets/mar_leg/resume", format!("csrf={csrf}")).await;
    assert!(body.contains("<table") && !body.contains("You're farming this"));
    assert!(!engine.read().await.configs[0].paused);
    // A bad token changes nothing and says why, in the panel.
    let (body, trigger) = post_as(&app, &cookie, "/markets/mar_leg/pause", "csrf=wrong&view=0xabc".into()).await;
    assert!(body.contains("You're farming this") && trigger.unwrap_or_default().contains("Invalid session"));
    assert!(!engine.read().await.configs[0].paused);

    // Remove from the panel: the leg is gone and the panel empties.
    let (body, trigger) = post_as(&app, &cookie, "/markets/mar_leg/remove", format!("csrf={csrf}&view=0xabc")).await;
    assert!(!body.contains("You're farming this") && trigger.is_none(), "{body}");
    assert!(engine.read().await.configs.is_empty());
}

/// An expired leg says so (not "waiting") and offers Extend, which opens the
/// edit drawer on Expires.
#[tokio::test]
async fn an_expired_leg_offers_extend_on_the_market_page() {
    use polyfarmer::types::OrderStatus;
    let (app, pw, engine, _) = app_with_leg(OrderStatus::Idle);
    engine.write().await.configs[0].expires_at = chrono::Utc::now() - chrono::Duration::hours(1);
    let cookie = login(&app, &pw).await;
    let (_, panel, _) = get_authed(&app, &cookie, "/markets/view/position?cid=0xabc").await;
    assert!(panel.contains(">Expired<") && !panel.contains("waiting for conditions"), "{panel}");
    assert!(panel.contains(r#"id="pos-mar_leg-extend""#) && panel.contains("/markets/mar_leg/edit?focus=expires"));
    let (_, drawer, _) = get_authed(&app, &cookie, "/markets/mar_leg/edit?focus=expires").await;
    assert!(drawer.contains(r#"id="e-exp" name="expires_in" data-autofocus"#), "{drawer}");
    let (_, drawer, _) = get_authed(&app, &cookie, "/markets/mar_leg/edit").await;
    assert!(!drawer.contains("data-autofocus"));
}

/// A two-leg market's panel pauses and resumes both legs together and stays
/// the panel; the action values are JSON-encoded, whatever the stored id.
#[tokio::test]
async fn the_market_page_panel_pauses_and_resumes_both_legs() {
    use polyfarmer::types::OrderStatus;
    let (app, pw, engine, _) = app_with_leg(OrderStatus::Idle);
    {
        let mut s = engine.write().await;
        let mut no = s.configs[0].clone();
        (no.id, no.token_id, no.token_label) = ("mar_leg_no".into(), "456".into(), "No".into());
        s.configs.push(no);
        s.order_status.insert("mar_leg_no".into(), OrderStatus::Idle);
    }
    let cookie = login(&app, &pw).await;
    let (_, page, rotated) = get_authed(&app, &cookie, "/markets").await;
    let cookie = rotated.unwrap_or(cookie);
    let csrf = extract_csrf(&page);
    let (_, panel, _) = get_authed(&app, &cookie, "/markets/view/position?cid=0xabc").await;
    assert!(panel.contains(r#"data-farmed="123,456""#) && panel.contains("Pause both"), "{panel}");
    assert!(
        panel.contains(r#"hx-vals="{&#34;csrf&#34;:"#) && panel.contains(r#"&#34;view&#34;:&#34;0xabc&#34;}""#),
        "{panel}"
    );

    let (body, _) = post_as(&app, &cookie, "/markets/group/0xabc/pause", format!("csrf={csrf}&view=0xabc")).await;
    assert!(body.contains("You're farming this") && body.contains("Resume both") && !body.contains("<table"));
    assert!(engine.read().await.configs.iter().all(|c| c.paused));
    let (body, _) = post_as(&app, &cookie, "/markets/group/0xabc/resume", format!("csrf={csrf}&view=0xabc")).await;
    assert!(body.contains("Pause both") && engine.read().await.configs.iter().all(|c| !c.paused));

    // A stored id with a quote can't break out of the JSON.
    engine.write().await.configs.iter_mut().for_each(|c| c.condition_id = r#"0x"evil":"1"#.into());
    let (_, panel, _) = get_authed(&app, &cookie, "/markets/view/position?cid=0x%22evil%22:%221").await;
    assert!(panel.contains(r#"&#34;view&#34;:&#34;0x\&#34;evil\&#34;:\&#34;1&#34;"#), "{panel}");
}

/// The market page's Trades and Top holders panels are login-only fragments,
/// and a bad market link answers with a message, not an error page, without
/// any request to Polymarket.
#[tokio::test]
async fn market_trades_and_holders_need_a_login_and_handle_bad_links() {
    let (app, pw) = test_app();
    for path in ["/markets/view/trades?slug=x&min=1000", "/markets/view/holders?slug=x"] {
        let res = app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert!(res.status().is_redirection(), "{path}: got {}", res.status());
        assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login", "{path}");
    }
    let cookie = login(&app, &pw).await;
    // Empty slug: refused before any network call. An unknown size filter
    // falls back to the default, so it can't be passed through to the API.
    let (status, trades, _) = get_authed(&app, &cookie, "/markets/view/trades?slug=&min=7").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        trades.contains("doesn&#39;t look like a Polymarket market link") || trades.contains("doesn't look like"),
        "{trades}"
    );
    let (status, holders, _) = get_authed(&app, &cookie, "/markets/view/holders?slug=").await;
    assert_eq!(status, StatusCode::OK);
    assert!(holders.contains("look like a Polymarket market link"), "{holders}");
    let (status, _, _) = get_authed(&app, &cookie, "/markets/view/trades?slug=&min=notanumber").await;
    assert!(status.is_client_error(), "a non-numeric filter is rejected: {status}");
}

/// A leg the engine paused for lack of funds shows "No balance" in the Markets
/// table and on the market page, and resuming it clears that (and its retry clock).
#[tokio::test]
async fn a_leg_paused_for_low_balance_says_so_until_resumed() {
    use polyfarmer::types::OrderStatus;
    let (app, pw, engine, _) = app_with_leg(OrderStatus::Idle);
    {
        let mut s = engine.write().await;
        s.configs[0].paused = true;
        s.low_balance_paused.insert("mar_leg".into());
        s.low_balance_since.insert("mar_leg".into(), std::time::Instant::now());
    }
    let cookie = login(&app, &pw).await;
    let (_, page, rotated) = get_authed(&app, &cookie, "/markets").await;
    let cookie = rotated.unwrap_or(cookie);
    let csrf = extract_csrf(&page);
    let (_, table, _) = get_authed(&app, &cookie, "/markets/table").await;
    assert!(table.contains(">No balance<") && table.contains("enough balance"), "{table}");
    let (_, panel, _) = get_authed(&app, &cookie, "/markets/view/position?cid=0xabc").await;
    assert!(panel.contains(">No balance<"), "{panel}");

    let (body, _) = post_as(&app, &cookie, "/markets/mar_leg/resume", format!("csrf={csrf}")).await;
    assert!(!body.contains("No balance"), "{body}");
    let s = engine.read().await;
    assert!(!s.configs[0].paused && s.low_balance_paused.is_empty() && s.low_balance_since.is_empty());
}

/// Pausing from the dashboard cancels whatever the leg holds at that moment:
/// a live order is claimed for cancelling, and a placement still in flight is
/// freed (the placing task then cancels what it gets back), never left as is.
#[tokio::test]
async fn pausing_claims_the_order_the_leg_holds_now() {
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    for (start, expect_cancelling) in [
        (OrderStatus::Live { order_id: "ord-now".into(), price: dec!(0.3) }, true),
        (OrderStatus::Placing { price: dec!(0.3) }, false),
    ] {
        let (app, pw, engine, _) = app_with_leg(start);
        let cookie = login(&app, &pw).await;
        let (_, page, rotated) = get_authed(&app, &cookie, "/markets").await;
        let cookie = rotated.unwrap_or(cookie);
        let csrf = extract_csrf(&page);
        post_as(&app, &cookie, "/markets/mar_leg/pause", format!("csrf={csrf}")).await;
        let s = engine.read().await;
        assert!(s.configs[0].paused);
        match s.order_status.get("mar_leg") {
            Some(OrderStatus::Cancelling { order_id, .. }) => assert!(expect_cancelling && order_id == "ord-now"),
            Some(OrderStatus::Idle) => assert!(!expect_cancelling, "a live order must be claimed, not dropped"),
            other => panic!("unexpected status {other:?}"),
        }
    }
}

type Engine = Arc<tokio::sync::RwLock<polyfarmer::engine::ws_manager::AppState>>;
type RaceTask = Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>;
type Race = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Make the dashboard's first markets.json write race `race` (another change
/// that saves markets.json): the write starts it and gives it up to 200 ms to
/// finish before writing. A dashboard that writes after releasing the engine
/// lock lets `race` save first and then overwrites it with an older list; one
/// that writes under the lock makes `race` wait, so its save lands last.
/// Returns the slot holding the race's task, for the test to await.
async fn race_first_markets_write(engine: &Engine, race: Race) -> RaceTask {
    let pending = std::sync::Mutex::new(Some(race));
    let task: RaceTask = Arc::new(std::sync::Mutex::new(None));
    let slot = Arc::clone(&task);
    let writer: polyfarmer::storage::MarketsWriter =
        Arc::new(move |path: &std::path::Path, configs: &[polyfarmer::types::MarketConfig]| {
            // Taken in its own statement so the lock is released at once: in
            // an `if let` it would be held for the whole block, and a racing
            // dashboard write would wait on it here.
            let race = pending.lock().unwrap().take();
            if let Some(race) = race {
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let handle = tokio::runtime::Handle::current().spawn(async move {
                    race.await;
                    let _ = done_tx.send(());
                });
                *slot.lock().unwrap() = Some(handle);
                // Blocks the test's own thread, not the runtime's workers
                // (multi-thread test), so `race` keeps running meanwhile.
                let _ = done_rx.recv_timeout(std::time::Duration::from_millis(200));
            }
            polyfarmer::storage::save_markets(path, configs)
        });
    engine.write().await.markets_writer = writer;
    task
}

/// An engine auto-pause of `id`, which must save.
fn engine_auto_pause(engine: &Engine, id: &'static str) -> Race {
    let engine = Arc::clone(engine);
    Box::pin(async move {
        let saved = polyfarmer::engine::placer::auto_pause(&engine, id).await;
        assert!(matches!(saved, Some((_, Ok(())))), "auto-pause saved");
    })
}

/// Await the race, then check markets.json holds exactly what is in memory.
async fn finish_race_and_check_disk(
    task: RaceTask,
    engine: &Engine,
    markets_file: &std::path::Path,
) -> Vec<polyfarmer::types::MarketConfig> {
    let handle = task.lock().unwrap().take().expect("the dashboard wrote markets.json");
    handle.await.unwrap();
    let memory = serde_json::to_value(&engine.read().await.configs).unwrap();
    let raw = std::fs::read_to_string(markets_file).unwrap();
    let disk: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(disk, memory, "markets.json must match memory");
    serde_json::from_value(disk).unwrap()
}

/// A second, running leg on another market.
async fn add_other_leg(engine: &Engine) {
    let mut s = engine.write().await;
    let mut other = s.configs[0].clone();
    other.id = "mar_other".into();
    other.condition_id = "0xdef".into();
    other.token_id = "456".into();
    other.paused = false;
    s.configs.push(other);
    s.order_status.insert("mar_other".into(), polyfarmer::types::OrderStatus::Idle);
}

/// A dashboard pause and an engine auto-pause of another leg at the same
/// moment: both pauses end up in markets.json, not just the dashboard's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dashboard_pause_keeps_an_engine_auto_pause_that_saved_meanwhile() {
    let (app, pw, engine, markets_file) = app_with_leg(polyfarmer::types::OrderStatus::Idle);
    add_other_leg(&engine).await;
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let task = race_first_markets_write(&engine, engine_auto_pause(&engine, "mar_other")).await;

    let (status, _) = post_form(&app, &cookie, "/markets/mar_leg/pause", format!("csrf={csrf}")).await;
    assert_eq!(status, StatusCode::OK);

    let saved = finish_race_and_check_disk(task, &engine, &markets_file).await;
    assert!(saved.iter().all(|c| c.paused), "both legs paused on disk: {saved:?}");
}

/// Resuming a leg while the engine auto-pauses it: whichever lands last in
/// memory is what markets.json says, so an auto-paused leg can't quote again
/// after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dashboard_resume_does_not_undo_an_engine_auto_pause_on_disk() {
    let (app, pw, engine, markets_file) = app_with_leg(polyfarmer::types::OrderStatus::Idle);
    engine.write().await.configs[0].paused = true;
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let task = race_first_markets_write(&engine, engine_auto_pause(&engine, "mar_leg")).await;

    let (status, _) = post_form(&app, &cookie, "/markets/mar_leg/resume", format!("csrf={csrf}")).await;
    assert_eq!(status, StatusCode::OK);

    let saved = finish_race_and_check_disk(task, &engine, &markets_file).await;
    assert!(saved[0].paused, "the auto-pause came last: {saved:?}");
}

/// An edit and an engine auto-pause of another leg at the same moment: both
/// changes end up in markets.json.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dashboard_edit_keeps_an_engine_auto_pause_that_saved_meanwhile() {
    let (app, pw, engine, markets_file, state) = app_state_with_leg(polyfarmer::types::OrderStatus::Idle);
    seed_book_and_min(&state, "123", rust_decimal_macros::dec!(0.40), rust_decimal_macros::dec!(5)).await;
    add_other_leg(&engine).await;
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let task = race_first_markets_write(&engine, engine_auto_pause(&engine, "mar_other")).await;

    let body =
        format!("csrf={csrf}&order_size=60&distance_cents=2&min_depth_usd=500&max_volatility_cents=&expires_in=keep");
    let (status, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", body).await;
    assert_eq!(status, StatusCode::OK, "{out}");

    let saved = finish_race_and_check_disk(task, &engine, &markets_file).await;
    let leg = saved.iter().find(|c| c.id == "mar_leg").unwrap();
    let other = saved.iter().find(|c| c.id == "mar_other").unwrap();
    assert_eq!(leg.order_size, rust_decimal_macros::dec!(60), "edit saved");
    assert!(other.paused, "auto-pause saved: {saved:?}");
}

/// A pause and a remove of another leg at the same moment, both from the
/// dashboard: the removed leg doesn't come back in markets.json.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dashboard_pause_does_not_bring_back_a_leg_removed_meanwhile() {
    let (app, pw, engine, markets_file) = app_with_leg(polyfarmer::types::OrderStatus::Idle);
    add_other_leg(&engine).await;
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let remove: Race = {
        let (app, cookie, csrf) = (app.clone(), cookie.clone(), csrf.clone());
        Box::pin(async move {
            let (status, _) = post_form(&app, &cookie, "/markets/mar_other/remove", format!("csrf={csrf}")).await;
            assert_eq!(status, StatusCode::OK);
        })
    };
    let task = race_first_markets_write(&engine, remove).await;

    let (status, _) = post_form(&app, &cookie, "/markets/mar_leg/pause", format!("csrf={csrf}")).await;
    assert_eq!(status, StatusCode::OK);

    let saved = finish_race_and_check_disk(task, &engine, &markets_file).await;
    assert_eq!(saved.len(), 1, "removed leg stays gone: {saved:?}");
    assert!(saved[0].id == "mar_leg" && saved[0].paused, "{saved:?}");
}

// ── Legs loaded paused because a value is invalid ──────────────────────────────

/// The error toast a dashboard action answered with, if any.
fn error_toast(trigger: Option<String>) -> String {
    let Some(trigger) = trigger else { return String::new() };
    let event: serde_json::Value = serde_json::from_str(&trigger).unwrap();
    event["pf-error"].as_str().unwrap_or_default().to_string()
}

/// A leg loaded paused with an invalid value (here a distance of 0) can't be
/// resumed until it is fixed: Resume says why and changes nothing, alone or
/// with its market's other leg. Once Edit saves valid values, it resumes.
#[tokio::test]
async fn an_invalid_leg_cannot_resume_until_an_edit_fixes_it() {
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    let (app, pw, engine, markets_file, state) = app_state_with_leg(OrderStatus::Idle);
    seed_book_and_min(&state, "123", dec!(0.40), dec!(5)).await;
    {
        let mut s = engine.write().await;
        let leg = &mut s.configs[0];
        leg.distance = dec!(0);
        leg.paused = true;
        let mut other = leg.clone();
        other.id = "mar_other".into();
        other.token_id = "456".into();
        other.token_label = "No".into();
        other.distance = dec!(0.02);
        s.configs.push(other);
    }
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;

    // Listed and paused; the table renders even with a zero tick and distance.
    engine.write().await.configs[0].tick_size = dec!(0);
    let (status, table, _) = get_authed(&app, &cookie, "/markets/table").await;
    assert_eq!(status, StatusCode::OK);
    assert!(table.contains("Will the test pass?") && table.contains("Paused"), "{table}");
    {
        let mut s = engine.write().await;
        s.configs[0].tick_size = dec!(0.01);
        polyfarmer::storage::save_markets(&markets_file, &s.configs).unwrap();
    }
    let before = std::fs::read_to_string(&markets_file).unwrap();

    for uri in ["/markets/mar_leg/resume", "/markets/group/0xabc/resume"] {
        let (_, trigger) = post_as(&app, &cookie, uri, format!("csrf={csrf}")).await;
        assert_eq!(
            error_toast(trigger),
            "Can't resume Will the test pass? (Yes): distance must be > 0, got 0. Fix it in Edit, or remove it.",
            "{uri}"
        );
        let s = engine.read().await;
        assert!(s.configs.iter().all(|c| c.paused), "{uri}: nothing resumed");
        drop(s);
        assert_eq!(std::fs::read_to_string(&markets_file).unwrap(), before, "{uri}: nothing saved");
    }

    let edit =
        format!("csrf={csrf}&order_size=100&distance_cents=2&min_depth_usd=500&max_volatility_cents=&expires_in=keep");
    let (status, _) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").distance, dec!(0.02));

    let (_, trigger) = post_as(&app, &cookie, "/markets/mar_leg/resume", format!("csrf={csrf}")).await;
    assert_eq!(error_toast(trigger), "", "resumed without an error");
    assert!(!leg_of(&*engine.read().await, "mar_leg").paused);
    let saved: Vec<polyfarmer::types::MarketConfig> =
        serde_json::from_str(&std::fs::read_to_string(&markets_file).unwrap()).unwrap();
    let saved_leg = saved.iter().find(|c| c.id == "mar_leg").unwrap();
    assert!(!saved_leg.paused && saved_leg.distance == dec!(0.02), "{saved_leg:?}");
}

/// A leg without a token id can't trade at all, and Edit can't give it one:
/// Resume and Edit both say to remove it (Edit before looking anything up on
/// Polymarket for token ""), and Remove works.
#[tokio::test]
async fn a_leg_without_a_token_id_can_only_be_removed() {
    use polyfarmer::types::OrderStatus;
    let (app, pw, engine, _) = app_with_leg(OrderStatus::Idle);
    {
        let mut s = engine.write().await;
        s.configs[0].token_id = String::new();
        s.configs[0].paused = true;
    }
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let (_, trigger) = post_as(&app, &cookie, "/markets/mar_leg/resume", format!("csrf={csrf}")).await;
    assert_eq!(
        error_toast(trigger),
        "Can't resume Will the test pass? (Yes): token_id is empty. Remove it and add the market again."
    );
    assert!(engine.read().await.configs[0].paused);

    // Setting an auto-pause limit would read the token's best bid first.
    let edit =
        format!("csrf={csrf}&order_size=100&distance_cents=2&min_depth_usd=500&max_volatility_cents=5&expires_in=keep");
    let (status, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit).await;
    assert_eq!(status, StatusCode::OK);
    assert!(out.contains("Remove it and add the market again."), "{out}");
    let s = engine.read().await;
    assert!(s.configs[0].paused && s.configs[0].max_volatility.is_none(), "edit refused");
    drop(s);

    let (_, trigger) = post_as(&app, &cookie, "/markets/mar_leg/remove", format!("csrf={csrf}")).await;
    assert_eq!(error_toast(trigger), "");
    assert!(engine.read().await.configs.is_empty());
}

// ── Edit and Polymarket's minimum order ─────────────────────────────────────

/// An edit of a leg to `size` dollars, `distance` cents and `depth` dollars ahead.
fn edit_leg(csrf: &str, size: &str, distance: &str, depth: &str) -> String {
    format!(
        "csrf={csrf}&order_size={size}&distance_cents={distance}&min_depth_usd={depth}&max_volatility_cents=&expires_in=keep"
    )
}

/// Polymarket rejects an order under the market's minimum size, and the leg
/// pauses after three rejections. So Edit refuses a size that would place one,
/// priced where the leg rests on the engine's live book, and saves nothing.
#[tokio::test]
async fn edit_refuses_a_size_below_the_minimum_order() {
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    let (app, pw, engine, markets_file, state) = app_state_with_leg(OrderStatus::Idle);
    seed_book_and_min(&state, "123", dec!(0.40), dec!(5)).await;
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;

    // $1 at 38¢ (2¢ below the 40¢ best bid) is 2.63 shares.
    let (status, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "1", "2", "500")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").order_size, dec!(100), "not changed");
    assert!(!markets_file.exists(), "nothing saved");
    assert!(out.contains("Too small for this market: $1.00 per side is 2.6 shares of Yes at 38¢;"), "{out}");
    assert!(out.contains("minimum order is 5 shares. Enter at least $1.90."), "{out}");
    assert!(out.contains(r#"value="1""#), "what was typed is kept");
}

/// Exactly the minimum is enough. A distance change moves the price, so it is
/// checked too: 1¢ closer makes the same $1.90 too small.
#[tokio::test]
async fn edit_accepts_the_minimum_order_and_checks_a_distance_change() {
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    let (app, pw, engine, _, state) = app_state_with_leg(OrderStatus::Idle);
    seed_book_and_min(&state, "123", dec!(0.40), dec!(5)).await;
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;

    // $1.90 at 38¢ is exactly 5 shares.
    let (status, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "1.90", "2", "500")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!out.contains("alert err"), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").order_size, dec!(1.90), "saved");

    // At 39¢ it is 4.87 shares.
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "1.90", "1", "500")).await;
    assert!(out.contains("4.8 shares of Yes at 39¢") && out.contains("Enter at least $1.95."), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").distance, dec!(0.02), "not changed");
}

/// The engine rests a leg at best bid − distance rounded down to the market's
/// tick, so a distance between ticks would quietly rest a tick further away
/// than typed. Edit refuses one, naming the tick, and the drawer steps by it.
#[tokio::test]
async fn edit_refuses_a_distance_between_ticks() {
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    let (app, pw, engine, markets_file, state) = app_state_with_leg(OrderStatus::Idle);
    seed_book_and_min(&state, "123", dec!(0.40), dec!(5)).await;
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;

    // A 1¢ tick.
    let (status, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "100", "1.55", "500")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").distance, dec!(0.02), "not changed");
    assert!(!markets_file.exists(), "nothing saved");
    assert!(out.contains("Distance must be in steps of 1¢ on this market."), "{out}");
    assert!(out.contains(r#"value="1.55""#), "what was typed is kept");
    let (_, drawer, _) = get_authed(&app, &cookie, "/markets/mar_leg/edit").await;
    assert!(drawer.contains(r#"name="distance_cents" type="number" step="1" min="1""#), "{drawer}");
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "100", "3", "500")).await;
    assert!(!out.contains("alert err"), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").distance, dec!(0.03), "on the tick: saved");

    // A 0.1¢ tick.
    engine.write().await.configs[0].tick_size = dec!(0.001);
    let (_, drawer, _) = get_authed(&app, &cookie, "/markets/mar_leg/edit").await;
    assert!(drawer.contains(r#"name="distance_cents" type="number" step="0.1" min="0.1""#), "{drawer}");
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "100", "1.55", "500")).await;
    assert!(out.contains("Distance must be in steps of 0.1¢ on this market."), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").distance, dec!(0.03), "not changed");
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "100", "1.5", "500")).await;
    assert!(!out.contains("alert err"), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").distance, dec!(0.015), "on the tick: saved");
}

/// A leg saved off its tick (before Edit checked it, or when the market's tick
/// grew) keeps its other edits: the tick is checked only when the distance
/// changes, and its drawer doesn't step by a tick its value isn't on.
#[tokio::test]
async fn a_leg_saved_off_its_tick_still_saves_other_edits() {
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    let (app, pw, engine, _, state) = app_state_with_leg(OrderStatus::Idle);
    {
        let mut s = engine.write().await;
        s.configs[0].distance = dec!(0.0155);
        s.configs[0].expires_at = chrono::Utc::now() + chrono::Duration::days(2);
    }
    seed_book_and_min(&state, "123", dec!(0.40), dec!(5)).await;
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;

    let (_, drawer, _) = get_authed(&app, &cookie, "/markets/mar_leg/edit").await;
    assert!(drawer.contains(r#"name="distance_cents" type="number" step="any""#), "{drawer}");

    // Expiry, depth ahead and auto-pause, distance as saved.
    let body = format!(
        "csrf={csrf}&order_size=100&distance_cents=1.55&min_depth_usd=900&max_volatility_cents=5&expires_in=30d"
    );
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", body).await;
    assert!(!out.contains("alert err"), "{out}");
    {
        let s = engine.read().await;
        let leg = leg_of(&s, "mar_leg");
        assert!(leg.expires_at > chrono::Utc::now() + chrono::Duration::days(29), "expiry saved");
        assert_eq!((leg.min_depth_between, leg.max_volatility), (dec!(900), Some(dec!(0.05))), "saved");
        assert_eq!(leg.distance, dec!(0.0155), "distance as it was");
    }
    // Size, distance as saved.
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "60", "1.55", "900")).await;
    assert!(!out.contains("alert err"), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").order_size, dec!(60), "size saved");

    // A new distance must be on the tick.
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "60", "2.55", "900")).await;
    assert!(out.contains("Distance must be in steps of 1¢ on this market."), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").distance, dec!(0.0155), "not changed");
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "60", "2", "900")).await;
    assert!(!out.contains("alert err"), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").distance, dec!(0.02), "back on the tick");
    let (_, drawer, _) = get_authed(&app, &cookie, "/markets/mar_leg/edit").await;
    assert!(drawer.contains(r#"name="distance_cents" type="number" step="1" min="1""#), "{drawer}");
}

/// Without the running engine's live book there is no price to check at: a
/// size or distance edit is refused until it loads. Other edits don't change
/// the order's shares, so they save without a book or a known minimum.
#[tokio::test]
async fn edit_without_a_live_book_refuses_only_size_and_distance_changes() {
    use polyfarmer::types::{EnginePhase, OrderStatus};
    use rust_decimal_macros::dec;
    let (app, pw, engine, _, state) = app_state_with_leg(OrderStatus::Idle);
    engine.write().await.engine_phase = EnginePhase::Running;
    state.caches.min_order_size.put("123", dec!(5));
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;

    for (size, distance) in [("60", "2"), ("100", "3")] {
        let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, size, distance, "500")).await;
        assert!(out.contains("loaded yet — try again in a few seconds."), "{size}/{distance}: {out}");
        let s = engine.read().await;
        let leg = leg_of(&s, "mar_leg");
        assert!(leg.order_size == dec!(100) && leg.distance == dec!(0.02), "not changed");
    }

    // A book without a best bid gives no price either.
    engine.write().await.books.insert("123".into(), Default::default());
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "60", "2", "500")).await;
    assert!(out.contains("loaded yet — try again in a few seconds."), "{out}");

    // Depth ahead only: no price or minimum needed, saved.
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "100", "2", "900")).await;
    assert!(!out.contains("alert err"), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").min_depth_between, dec!(900), "saved");
}

/// With no engine (no wallet yet, or a failed start) no book will ever load,
/// so the refusal says to start the bot instead of to wait. A starting engine
/// is about to load it: wait.
#[tokio::test]
async fn edit_without_a_running_bot_says_to_start_it() {
    use polyfarmer::types::{EnginePhase, OrderStatus};
    use rust_decimal_macros::dec;
    let (app, pw, engine, _, state) = app_state_with_leg(OrderStatus::Idle);
    state.caches.min_order_size.put("123", dec!(5));
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;

    let not_running = "no live order book to check the size against — start the bot (Settings), then edit.";
    for phase in [EnginePhase::AwaitingWallet, EnginePhase::Error] {
        engine.write().await.engine_phase = phase.clone();
        let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "60", "2", "500")).await;
        assert!(out.contains("The bot isn") && out.contains(not_running), "{phase:?}: {out}");
        assert!(!out.contains("loaded yet"), "{phase:?}: {out}");
        assert_eq!(leg_of(&*engine.read().await, "mar_leg").order_size, dec!(100), "{phase:?}: not changed");
    }

    engine.write().await.engine_phase = EnginePhase::Starting;
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "60", "2", "500")).await;
    assert!(out.contains("loaded yet — try again in a few seconds."), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").order_size, dec!(100), "not changed");
}

/// The minimum comes from Polymarket when it isn't cached. A failed read is
/// not a pass: the edit is refused. A minimum of 0 means there is none.
#[tokio::test]
async fn edit_refuses_when_the_minimum_cannot_be_read_and_a_zero_minimum_is_none() {
    use polyfarmer::types::OrderStatus;
    use rust_decimal_macros::dec;
    let (app, pw, engine, _, state) = app_state_with_leg(OrderStatus::Idle);
    // A token id Polymarket's client refuses before sending anything: the read
    // fails offline.
    engine.write().await.configs[0].token_id = "not-a-token".into();
    let book = polyfarmer::engine::orderbook::TokenBook { best_bid: Some(dec!(0.40)), ..Default::default() };
    engine.write().await.books.insert("not-a-token".into(), book);
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;

    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "60", "2", "500")).await;
    assert!(out.contains("minimum order size — try again."), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").order_size, dec!(100), "not changed");

    state.caches.min_order_size.put("not-a-token", dec!(0));
    let (_, out) = post_form(&app, &cookie, "/markets/mar_leg/edit", edit_leg(&csrf, "0.5", "2", "500")).await;
    assert!(!out.contains("alert err"), "{out}");
    assert_eq!(leg_of(&*engine.read().await, "mar_leg").order_size, dec!(0.5), "saved");
}

/// Logging out while another request of the same session is still running must
/// end the session for good. tower-sessions saves that request's copy of the
/// session when it finishes, after logout deleted it; the save must not bring
/// the session back. The "slow" request is a real `/markets` GET that passed the
/// auth guard and is held inside its handler by a write lock on the engine state.
#[tokio::test]
async fn logout_sticks_even_when_a_slow_request_finishes_after_it() {
    use std::time::Duration;
    let limit = Duration::from_secs(10);
    let store = CredentialStore::open(unique_dir()).unwrap();
    let pw = "test-password-123";
    store.set_password(pw).unwrap();
    let engine = empty_engine();
    let app = router(WebState::new(Arc::new(store), engine.clone()));

    let cookie = login(&app, pw).await;
    let (status, page, rotated) = get_authed(&app, &cookie, "/markets").await;
    assert_eq!(status, StatusCode::OK);
    let cookie = rotated.unwrap_or(cookie);
    let csrf = extract_csrf(&page);

    let hold = engine.write().await;
    let slow = tokio::spawn({
        let (app, cookie) = (app.clone(), cookie.clone());
        async move { get_authed(&app, &cookie, "/markets").await.0 }
    });
    // Let the slow request run until it blocks on the engine lock, past the auth guard.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(!slow.is_finished(), "the slow request must be held open inside its handler");

    let res = tokio::time::timeout(
        limit,
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/logout")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={csrf}")))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");
    let (status, _, _) = tokio::time::timeout(limit, get_authed(&app, &cookie, "/markets")).await.unwrap();
    assert!(status.is_redirection(), "signed out right after logout, got {status}");

    drop(hold);
    let slow_status = tokio::time::timeout(limit, slow).await.unwrap().unwrap();
    assert_eq!(slow_status, StatusCode::OK, "the request that started before logout completes normally");

    let res = tokio::time::timeout(
        limit,
        app.clone()
            .oneshot(Request::builder().uri("/markets").header(header::COOKIE, &cookie).body(Body::empty()).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(res.status().is_redirection(), "the old cookie must stay signed out, got {}", res.status());
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");
}

// ── a session that ends while htmx is polling ──────────────────────────────────

/// Send `method uri` as htmx does (`HX-Request: true`), optionally with a session cookie.
async fn htmx_request(app: &Router, method: &str, uri: &str, cookie: Option<&str>) -> axum::response::Response {
    let mut req = Request::builder().method(method).uri(uri).header("HX-Request", "true");
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, cookie);
    }
    app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
}

/// A refused htmx request must make htmx load `dest` as a whole page. A 3xx
/// would not: the browser follows it inside the XHR and htmx swaps the login
/// page into the polled fragment. htmx acts on `HX-Redirect` for any non-3xx
/// status before its swap and error handling; 204 leaves nothing to swap.
async fn assert_navigates_whole_page(res: axum::response::Response, dest: &str, what: &str) {
    assert_eq!(res.status(), StatusCode::NO_CONTENT, "{what}");
    assert_eq!(header_str(&res, "hx-redirect").as_deref(), Some(dest), "{what}");
    assert!(res.headers().get(header::LOCATION).is_none(), "{what}: no redirect for the XHR to follow");
    assert_eq!(body_string(res).await, "", "{what}: nothing to swap into the fragment");
}

#[tokio::test]
async fn htmx_requests_without_a_session_send_the_whole_page_to_login() {
    let (app, _) = test_app();
    for (method, uri) in [
        ("GET", "/markets/table"),
        ("GET", "/status/strip"),
        ("GET", "/overview/kpis"),
        ("POST", "/markets/some-leg/pause"),
    ] {
        let res = htmx_request(&app, method, uri, None).await;
        assert_navigates_whole_page(res, "/login", &format!("{method} {uri}")).await;
    }
}

#[tokio::test]
async fn htmx_poll_after_logout_sends_the_whole_page_to_login() {
    let (app, pw) = test_app();
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let res = htmx_request(&app, "GET", "/markets/table", Some(&cookie)).await;
    assert_eq!(res.status(), StatusCode::OK, "signed in: the fragment renders");
    assert!(res.headers().get("hx-redirect").is_none());

    let (status, _) = post_form(&app, &cookie, "/logout", format!("csrf={csrf}")).await;
    assert!(status.is_redirection());

    let res = htmx_request(&app, "GET", "/markets/table", Some(&cookie)).await;
    assert_navigates_whole_page(res, "/login", "poll after logout").await;
    let (status, _, _) = get_authed(&app, &cookie, "/markets").await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a normal page load keeps the plain redirect");
}

#[tokio::test]
async fn htmx_requests_on_first_run_send_the_whole_page_to_welcome() {
    let (app, _) = test_app_uninit_with_code();
    for (method, uri) in [("GET", "/status/strip"), ("POST", "/markets/some-leg/pause")] {
        let res = htmx_request(&app, method, uri, None).await;
        assert_navigates_whole_page(res, "/welcome", &format!("{method} {uri}")).await;
    }
    let res = app.oneshot(Request::builder().uri("/markets/table").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "non-htmx requests keep the plain redirect");
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/welcome");
}

// ── Parallel guessing can't outrun the attempt limiter ──────────────────────────

const BURST: usize = 50;

/// Send `BURST` requests built by `make` at the same instant; returns the bodies.
async fn burst(app: &Router, make: impl Fn() -> Request<Body>) -> Vec<String> {
    let start = Arc::new(tokio::sync::Barrier::new(BURST));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..BURST {
        let (app, req, start) = (app.clone(), make(), Arc::clone(&start));
        set.spawn(async move {
            start.wait().await;
            body_string(app.oneshot(req).await.unwrap()).await
        });
    }
    set.join_all().await
}

/// Fire bursts of wrong guesses until one is refused entirely; returns how many
/// guesses were actually tried (answered `wrong`). Every other answer must be the
/// lockout message.
async fn guesses_before_lockout(app: &Router, wrong: &str, make: impl Fn() -> Request<Body>) -> usize {
    let mut tried = 0;
    for _ in 0..20 {
        let bodies = burst(app, &make).await;
        let wrong_now = bodies.iter().filter(|b| b.contains(wrong)).count();
        for b in bodies.iter().filter(|b| !b.contains(wrong)) {
            assert!(b.contains("Too many attempts"), "neither a tried guess nor the lockout: {b}");
        }
        tried += wrong_now;
        if wrong_now == 0 {
            return tried;
        }
    }
    panic!("the lockout never engaged ({tried} guesses tried)");
}

fn form_post(uri: &str, cookie: &str, body: String, ip: Option<&str>) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    if let Some(ip) = ip {
        req.extensions_mut().insert(peer(ip));
    }
    req
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_wrong_passwords_get_no_more_guesses_than_sequential_ones() {
    let (app, pw) = test_app();
    let res = app.clone().oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
    let cookie = session_cookie(&res).unwrap();
    let csrf = extract_csrf(&body_string(res).await);

    // Proxied (no peer address): the same 15 guesses as one at a time, then locked.
    let tried = guesses_before_lockout(&app, "Incorrect password", || {
        form_post("/login", &cookie, format!("csrf={csrf}&password=definitely-wrong"), None)
    })
    .await;
    assert_eq!(tried, 15, "guesses hashed before the lockout");
    let res = app.clone().oneshot(form_post("/login", &cookie, format!("csrf={csrf}&password={pw}"), None)).await;
    assert!(body_string(res.unwrap()).await.contains("Too many attempts"), "locked, even for the right password");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_wrong_passwords_from_one_ip_lock_only_that_ip_after_five() {
    let (app, pw) = test_app();
    let res = app.clone().oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
    let cookie = session_cookie(&res).unwrap();
    let csrf = extract_csrf(&body_string(res).await);

    let tried = guesses_before_lockout(&app, "Incorrect password", || {
        form_post("/login", &cookie, format!("csrf={csrf}&password=definitely-wrong"), Some("203.0.113.7"))
    })
    .await;
    assert_eq!(tried, 5, "guesses hashed before that IP was locked");
    let res = post_login_from(&app, "203.0.113.99", &cookie, &csrf, &pw).await;
    assert!(res.status().is_redirection(), "another IP still signs in, got {}", res.status());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_wrong_current_passwords_are_throttled_like_login() {
    let (app, pw) = test_app();
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    let form =
        format!("csrf={csrf}&current=definitely-wrong&password=a-brand-new-password&confirm=a-brand-new-password");
    let tried = guesses_before_lockout(&app, "Current password is incorrect", || {
        form_post("/setup/password", &cookie, form.clone(), None)
    })
    .await;
    assert_eq!(tried, 15, "re-authentication guesses hashed before the lockout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_wrong_setup_codes_are_throttled_like_sequential_ones() {
    let (app, _) = test_app_uninit_with_code();
    let (cookie, csrf, _) = welcome_page(&app, "/welcome").await;
    let pw = "supersecret-pass-1";
    let tried = guesses_before_lockout(&app, "Setup code is incorrect", || {
        form_post("/welcome", &cookie, format!("csrf={csrf}&code=zzzz-zzzz&password={pw}&confirm={pw}"), None)
    })
    .await;
    assert_eq!(tried, 15, "setup-code guesses checked before the lockout");
}

// ── Live streams end with the session ─────────────────────────────────────────

/// An app with its state (to send stream events), signed in: (router, state, cookie, CSRF token, password).
async fn signed_in_app() -> (Router, WebState, String, String, String) {
    let store = CredentialStore::open(unique_dir()).unwrap();
    let pw = "test-password-123".to_string();
    store.set_password(&pw).unwrap();
    let state = WebState::new(Arc::new(store), empty_engine());
    let app = router(state.clone());
    let (cookie, csrf) = authed_with_csrf(&app, &pw).await;
    (app, state, cookie, csrf, pw)
}

type EventStream = axum::body::BodyDataStream;

async fn open_events(app: &Router, cookie: &str) -> EventStream {
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/events").header(header::COOKIE, cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    res.into_body().into_data_stream()
}

/// What the stream does within `within`: `Ok(Some(text))` sent an event,
/// `Ok(None)` ended, `Err(())` still open (keep-alive comments don't count).
async fn next_from(events: &mut EventStream, within: std::time::Duration) -> Result<Option<String>, ()> {
    use futures_util::StreamExt;
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, events.next()).await {
            Ok(Some(chunk)) => {
                let text = String::from_utf8(chunk.unwrap().to_vec()).unwrap();
                if !text.starts_with(':') {
                    return Ok(Some(text));
                }
            }
            Ok(None) => return Ok(None),
            Err(_) => return Err(()),
        }
    }
}

/// Logout deletes the session from the store while this stream still holds the
/// session it loaded at connect: the stream must see the store, and end before
/// sending the next event.
#[tokio::test]
async fn the_event_stream_ends_at_its_next_event_after_logout() {
    let (app, state, cookie, csrf, _) = signed_in_app().await;
    let mut events = open_events(&app, &cookie).await;
    state.state_tx.send(()).unwrap();
    let first = next_from(&mut events, std::time::Duration::from_secs(2)).await;
    assert!(matches!(&first, Ok(Some(t)) if t.contains("event: state")), "signed in, it streams: {first:?}");

    let (status, _) = post_form(&app, &cookie, "/logout", format!("csrf={csrf}")).await;
    assert!(status.is_redirection());
    state.state_tx.send(()).unwrap();
    let after = next_from(&mut events, std::time::Duration::from_secs(2)).await;
    assert_eq!(after, Ok(None), "the stream must end after logout, not keep sending");
}

/// With nothing to send, a signed-out stream still ends within 15 seconds.
#[tokio::test]
async fn an_idle_event_stream_ends_within_seconds_after_logout() {
    let (app, _state, cookie, csrf, _) = signed_in_app().await;
    tokio::time::pause(); // after the login: its password hashing must not race the clock
    let mut events = open_events(&app, &cookie).await;
    let (status, _) = post_form(&app, &cookie, "/logout", format!("csrf={csrf}")).await;
    assert!(status.is_redirection());
    let after = next_from(&mut events, std::time::Duration::from_secs(15)).await;
    assert_eq!(after, Ok(None), "an idle stream must end within 15s of logout");
}

/// A password change signs out every other session, and their streams with them.
#[tokio::test]
async fn the_event_stream_of_another_session_ends_after_a_password_change() {
    let (app, state, owner, csrf, pw) = signed_in_app().await;
    let (other, _) = authed_with_csrf(&app, &pw).await;
    let mut events = open_events(&app, &other).await;

    let form = format!("csrf={csrf}&current={pw}&password=a-brand-new-password&confirm=a-brand-new-password");
    let (status, page) = post_form(&app, &owner, "/setup/password", form).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Password changed"), "{page}");
    state.state_tx.send(()).unwrap();
    let after = next_from(&mut events, std::time::Duration::from_secs(2)).await;
    assert_eq!(after, Ok(None), "the other session's stream must end, not keep sending");
}
