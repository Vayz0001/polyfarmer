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
        async move { app.oneshot(Request::builder().uri(uri).header(header::COOKIE, cookie).body(Body::empty()).unwrap()).await.unwrap() }
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
    assert_eq!(get(a_new, "/markets").await.status(), StatusCode::OK, "the session that changed the password stays signed in");
}

#[tokio::test]
async fn new_passwords_must_meet_the_length_policy() {
    let (app, pw) = test_app();
    let cookie = login(&app, &pw).await;
    let page = app.clone().oneshot(Request::builder().uri("/setup").header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    let cookie = session_cookie(&page).unwrap_or(cookie);
    let csrf = extract_csrf(&body_string(page).await);
    for (new, expect) in [("short", "at least 12"), ("elevenchars", "at least 12"), (&"x".repeat(129)[..], "at most 128")] {
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
