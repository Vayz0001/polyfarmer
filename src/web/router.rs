//! axum router: public routes (login, assets), protected routes (dashboard,
//! setup) behind the auth guard, with a session layer over everything.

use axum::{
    routing::{get, post},
    Router,
};
use tower_sessions::cookie::SameSite;
use tower_sessions::{MemoryStore, SessionManagerLayer};

use super::state::WebState;
use super::{activity, assets, auth, dashboard, events, markets, positions, rewards, setup, shell};

/// Build the dashboard router with shared [`WebState`].
pub fn router(state: WebState) -> Router {
    // In-memory sessions: fine for a single-user self-hosted app (re-login on
    // restart). `Secure` is opt-in (DASHBOARD_SECURE_COOKIES): off for plain
    // http://localhost, on when served over HTTPS (Tailscale Serve, a proxy).
    let session_layer = SessionManagerLayer::new(MemoryStore::default())
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_secure(state.secure_cookies);

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
        .with_state(state)
}
