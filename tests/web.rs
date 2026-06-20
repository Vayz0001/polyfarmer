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
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("pf-web-test-{n}"))
}

/// Build a router backed by a fresh store; returns the router + admin password.
fn test_app() -> (Router, String) {
    let store = CredentialStore::open(unique_dir()).unwrap();
    let pw = store.init_admin().unwrap();
    (router(WebState::new(Arc::new(store))), pw)
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
    assert!(body_string(res).await.contains("Wallet"));
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
