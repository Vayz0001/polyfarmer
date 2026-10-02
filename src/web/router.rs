//! axum router: public routes (login, assets), protected routes (dashboard,
//! setup) behind the auth guard, with a session layer over everything.

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use tower_sessions::cookie::SameSite;
use tower_sessions::{MemoryStore, SessionManagerLayer};

use super::state::WebState;
use super::{activity, assets, auth, dashboard, events, markets, positions, rewards, setup, shell};

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
        .route("/", get(dashboard::overview))
        .route("/overview/kpis", get(dashboard::overview_kpis))
        .route("/overview/book", get(dashboard::overview_book))
        .route("/overview/fills", get(dashboard::overview_fills))
        .route("/status/strip", get(shell::status_strip))
        .route("/events", get(events::stream))
        .route("/markets", get(dashboard::markets))
        .route("/markets/table", get(dashboard::markets_table))
        .route("/markets/browse", get(markets::browse_page))
        .route("/markets/browse/results", get(markets::browse_results))
        .route("/markets/view", get(markets::market_view))
        .route("/markets/view/book", get(markets::view_book))
        .route("/markets/view/book/stream", get(markets::view_book_stream))
        .route("/markets/view/chart", get(markets::view_chart))
        .route("/markets/view/position", get(markets::view_position))
        .route("/markets/view/preview", post(markets::view_preview))
        .route("/markets/start", post(markets::start_farming))
        .route("/markets/resolve", get(markets::resolve_url))
        .route("/markets/{id}/edit", get(markets::edit_form).post(markets::edit_submit))
        .route("/markets/{id}/remove", post(markets::remove_market))
        .route("/markets/{id}/pause", post(markets::pause_market))
        .route("/markets/{id}/resume", post(markets::resume_market))
        .route("/markets/group/{cid}/pause", post(markets::pause_group))
        .route("/markets/group/{cid}/resume", post(markets::resume_group))
        .route("/positions", get(positions::page))
        .route("/positions/table", get(positions::table))
        .route("/activity", get(activity::page))
        .route("/activity/recent", get(activity::recent))
        .route("/rewards", get(rewards::page))
        .route("/rewards/table", get(rewards::table))
        .route("/rewards/alltime", get(rewards::alltime))
        .route("/rewards/history", get(rewards::history_page))
        .route("/setup", get(setup::page))
        .route("/setup/password", post(setup::set_password))
        .route("/setup/wallet", post(setup::set_wallet))
        .route("/setup/wallet/detect", post(setup::detect_wallet))
        .route("/setup/engine-status", get(setup::engine_status))
        .route("/launching", get(setup::launching))
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
