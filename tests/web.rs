//! Integration tests for the dashboard router — exercised via `oneshot`
//! (Askama rendering + rust-embed asset serving) with no network bind.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use polyfarmer::web::router;
use tower::ServiceExt; // for `oneshot`

#[tokio::test]
async fn index_serves_html() {
    let res = router()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains("polyfarmer"), "index should render the brand");
    assert!(body.contains("/assets/htmx.min.js"), "index should reference embedded htmx");
}

#[tokio::test]
async fn embedded_css_is_served() {
    let res = router()
        .oneshot(
            Request::builder()
                .uri("/assets/app.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn embedded_htmx_is_served() {
    let res = router()
        .oneshot(
            Request::builder()
                .uri("/assets/htmx.min.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_asset_is_404() {
    let res = router()
        .oneshot(
            Request::builder()
                .uri("/assets/nope.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
