//! axum router: public routes (login, assets), protected routes (dashboard,
//! setup) behind the auth guard, with a session layer over everything.

use axum::{
    routing::{get, post},
    Router,
};
use tower_sessions::cookie::SameSite;
use tower_sessions::{Expiry, SessionManagerLayer};

use super::auth::ANONYMOUS_TIMEOUT;
use super::security::harden;
use super::session_store::SWEEP_INTERVAL;
use super::state::WebState;
use super::{activity, assets, auth, dashboard, events, markets, positions, rewards, setup, shell};

/// Build the dashboard router with shared [`WebState`].
pub fn router(state: WebState) -> Router {
    // Sessions live in a bounded, self-cleaning in-memory store (re-login on
    // restart is fine for a single-user app). `Secure` is opt-in
    // (DASHBOARD_SECURE_COOKIES): off for plain http://localhost, on over HTTPS
    // (Tailscale Serve, a proxy) — where the cookie also gets the `__Host-`
    // prefix, which browsers only accept if it is Secure, Path=/ and Domain-less.
    state.sessions.spawn_sweeper(SWEEP_INTERVAL);
    let cookie_name = if state.secure_cookies { "__Host-polyfarmer" } else { "polyfarmer_session" };
    let session_layer = SessionManagerLayer::new(state.sessions.clone())
        .with_name(cookie_name)
        .with_path("/")
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_secure(state.secure_cookies)
        // Visitors who haven't logged in only hold a CSRF token: short-lived.
        // Logging in switches the session to the longer idle timeout (see auth).
        .with_expiry(Expiry::OnInactivity(time::Duration::seconds(ANONYMOUS_TIMEOUT.as_secs() as i64)))
        // Re-save on every request so the idle timeout is *sliding* (measured
        // from the last request, not from the last change to the session).
        .with_always_save(true);

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
        .route("/setup", get(setup::page))
        .route("/setup/password", post(setup::set_password))
        .route("/setup/wallet", post(setup::set_wallet))
        .route("/setup/wallet/detect", post(setup::detect_wallet))
        .route("/setup/engine-status", get(setup::engine_status))
        .route("/launching", get(setup::launching))
        .route("/logout", post(auth::logout))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth::require_auth));

    let public: Router<WebState> = Router::new()
        .route("/welcome", get(auth::welcome_form).post(auth::welcome_submit))
        .route("/login", get(auth::login_form).post(auth::login_submit))
        .route("/assets/{*path}", get(assets::serve));

    let https = state.secure_cookies;
    let app = Router::new().merge(protected).merge(public).layer(session_layer).with_state(state);
    // Security headers + CSP, cross-site write filter, body cap, panic containment.
    harden(app, https)
}
