//! axum router: public routes (login, assets), protected routes (dashboard,
//! setup) behind the auth guard, with a session layer over everything.

use askama::Template;
use axum::{
    extract::State,
    response::Html,
    routing::{get, post},
    Router,
};
use tower_sessions::cookie::SameSite;
use tower_sessions::{MemoryStore, Session, SessionManagerLayer};

use super::state::WebState;
use super::{assets, auth, setup};

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    version: &'static str,
    has_wallet: bool,
    must_change: bool,
}

async fn index(State(state): State<WebState>, _session: Session) -> Html<String> {
    let tpl = IndexTemplate {
        version: env!("CARGO_PKG_VERSION"),
        has_wallet: state.store.has_wallet(),
        must_change: state.store.must_change_password().unwrap_or(false),
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

/// Build the dashboard router with shared [`WebState`].
pub fn router(state: WebState) -> Router {
    // In-memory sessions: fine for a single-user self-hosted app (re-login on
    // restart). `secure` is off for localhost http; set true behind HTTPS.
    let session_layer = SessionManagerLayer::new(MemoryStore::default())
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_secure(false);

    let protected: Router<WebState> = Router::new()
        .route("/", get(index))
        .route("/setup", get(setup::page))
        .route("/setup/password", post(setup::set_password))
        .route("/setup/wallet", post(setup::set_wallet))
        .route("/logout", post(auth::logout))
        .route_layer(axum::middleware::from_fn(auth::require_auth));

    let public: Router<WebState> = Router::new()
        .route("/login", get(auth::login_form).post(auth::login_submit))
        .route("/assets/{*path}", get(assets::serve));

    Router::new()
        .merge(protected)
        .merge(public)
        .layer(session_layer)
        .with_state(state)
}
