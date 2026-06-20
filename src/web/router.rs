//! axum router + page handlers.

use askama::Template;
use axum::{response::Html, routing::get, Router};

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    title: &'static str,
    version: &'static str,
}

async fn index() -> Html<String> {
    let tpl = IndexTemplate {
        title: "polyfarmer",
        version: env!("CARGO_PKG_VERSION"),
    };
    Html(
        tpl.render()
            .unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")),
    )
}

/// Build the dashboard router. (Stateless for now; shared AppState wired in a later segment.)
pub fn router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(super::assets::serve))
}
