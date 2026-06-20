//! axum router: public routes (login, assets), protected routes (dashboard,
//! setup) behind the auth guard, with a session layer over everything.

use std::net::SocketAddr;

use askama::Template;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{Html, IntoResponse, Response},
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
}

async fn index(State(state): State<WebState>, _session: Session) -> Html<String> {
    let tpl = IndexTemplate {
        version: env!("CARGO_PKG_VERSION"),
        has_wallet: state.store.has_wallet(),
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

/// Until an admin password is set, only loopback clients may reach the dashboard
/// — so the first-run setup window can't be hijacked even if bound to 0.0.0.0.
/// (Connect info is absent in tests → allowed; present for the real server.)
async fn require_local_until_setup(
    State(state): State<WebState>,
    req: Request,
    next: Next,
) -> Response {
    if !state.store.is_initialized() {
        let is_local = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip().is_loopback())
            .unwrap_or(true);
        if !is_local {
            return (
                StatusCode::FORBIDDEN,
                "Setup is restricted to localhost until an admin password is set.",
            )
                .into_response();
        }
    }
    next.run(req).await
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
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let public: Router<WebState> = Router::new()
        .route("/welcome", get(auth::welcome_form).post(auth::welcome_submit))
        .route("/login", get(auth::login_form).post(auth::login_submit))
        .route("/assets/{*path}", get(assets::serve));

    Router::new()
        .merge(protected)
        .merge(public)
        .layer(session_layer)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_local_until_setup,
        ))
        .with_state(state)
}
