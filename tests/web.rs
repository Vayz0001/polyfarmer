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
    assert!(header_str(res, "permissions-policy").unwrap().contains("camera=()"), "{what}");
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
        (start("18", "100", "99999999999999999999", "", "7d"), "Min depth"),
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
        (edit("100", "2", "-1", "", "keep"), "Min depth"),
        (edit("100", "2", "500", "5000", "keep"), "Auto-pause"),
    ] {
        let (status, out) = post_form(&app, &cookie, "/markets/mar_test_leg/edit", body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(out.contains(expect), "expected {expect:?} for {body}\n got: {out}");
        assert!(!out.contains("Internal error"), "panicked for {body}");
    }
    // None of the rejected edits changed the saved leg (still $100, 2¢, $500 depth).
    let (_, table, _) = get_authed(&app, &cookie, "/markets/table").await;
    assert!(table.contains("$100") && table.contains("$500.00") && table.contains("2.0¢ below bid"), "{table}");
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
