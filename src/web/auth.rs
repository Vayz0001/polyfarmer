//! Authentication: session login/logout, the route guard, CSRF, and failed-
//! attempt throttling.
//!
//! Sessions are server-side (a bounded in-memory store, see `session_store`);
//! the cookie is HttpOnly + SameSite=Lax (+ `__Host-` and Secure over HTTPS).
//! Session hygiene follows the OWASP Session Management guidance:
//!
//! * the session id is **rotated on login** and on password change (fixation);
//! * an **idle timeout** (sliding) and an **absolute maximum age** both apply;
//! * every session carries the **password epoch** it was issued under, so
//!   changing the password logs out every other session (stolen-cookie recovery);
//! * password hashing runs on the blocking pool, not on an async worker thread.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use askama::Template;
use axum::{
    extract::{ConnectInfo, FromRequestParts, Query, Request, State},
    http::request::Parts,
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::Deserialize;
use tower_sessions::{Expiry, Session};

use crate::creds::validate_new_password;

use super::limiter::Source;
use super::session_store::AUTH_KEY;
use super::state::WebState;

const SESSION_CSRF: &str = "csrf";
/// Password epoch the session was issued under (see `CredentialStore::session_epoch`).
const SESSION_EPOCH: &str = "auth_epoch";
/// Unix seconds when the session logged in (for the absolute maximum age).
const SESSION_SINCE: &str = "auth_since";

/// Logged-in sessions expire after this long without activity (sliding).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(12 * 3600);
/// …and never live longer than this, however active.
pub const MAX_SESSION_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Visitors who have not logged in (they only hold a CSRF token) are dropped quickly.
pub const ANONYMOUS_TIMEOUT: Duration = Duration::from_secs(30 * 60);

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The connecting peer's address, if the server provides it. Behind a reverse
/// proxy / Tailscale Serve this is the proxy (loopback) for everyone.
pub struct ClientIp(pub Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(ClientIp(parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip())))
    }
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    csrf_token: String,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginForm {
    csrf: String,
    password: String,
}

// ── attempt throttling (login, setup-code entry, re-authentication) ──────────

/// Message for a locked-out source, or `None` if it may try.
fn lockout_message(state: &WebState, source: Source) -> Option<String> {
    state.limiter.check(source).map(|wait| {
        format!("Too many attempts — try again in {} seconds.", wait.as_secs().max(1))
    })
}

/// Verify the admin password on the blocking pool (argon2 is deliberately slow
/// and must not stall the async runtime that also drives the trading engine).
pub async fn verify_password_blocking(state: &WebState, password: &str) -> bool {
    let store = Arc::clone(&state.store);
    let password = password.to_string();
    tokio::task::spawn_blocking(move || store.verify_login(&password).unwrap_or(false))
        .await
        .unwrap_or(false)
}

/// Set the admin password on the blocking pool.
pub async fn set_password_blocking(state: &WebState, password: &str) -> eyre::Result<()> {
    let store = Arc::clone(&state.store);
    let password = password.to_string();
    tokio::task::spawn_blocking(move || store.set_password(&password))
        .await
        .map_err(|e| eyre::eyre!("hashing task failed: {e}"))?
}

// Re-exported so the settings handler can throttle its "current password" check too.
pub use super::limiter::Source as AttemptSource;

// ── CSRF ──────────────────────────────────────────────────────────────────────

/// Fetch (or lazily create) the per-session CSRF token.
pub async fn csrf_token(session: &Session) -> String {
    if let Ok(Some(token)) = session.get::<String>(SESSION_CSRF).await {
        return token;
    }
    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let _ = session.insert(SESSION_CSRF, token.clone()).await;
    token
}

pub async fn verify_csrf(session: &Session, provided: &str) -> bool {
    matches!(session.get::<String>(SESSION_CSRF).await, Ok(Some(t)) if t == provided)
}

// ── session state ─────────────────────────────────────────────────────────────

/// The pure part of "is this session still good?": logged in, issued under the
/// current password epoch, and within the absolute maximum age.
pub(crate) fn session_is_valid(
    auth: bool,
    epoch: Option<u64>,
    since: Option<i64>,
    current_epoch: u64,
    now: i64,
) -> bool {
    auth && epoch == Some(current_epoch)
        && since.is_some_and(|s| now >= s && (now - s) as u64 <= MAX_SESSION_AGE.as_secs())
}

/// Start an authenticated session: rotate the id (fixation), stamp the current
/// password epoch and login time, and switch to the longer idle timeout.
pub async fn begin_session(session: &Session, state: &WebState) {
    let _ = session.cycle_id().await;
    let _ = session.insert(AUTH_KEY, true).await;
    let _ = session.insert(SESSION_EPOCH, state.store.session_epoch()).await;
    let _ = session.insert(SESSION_SINCE, unix_now()).await;
    session.set_expiry(Some(Expiry::OnInactivity(time::Duration::seconds(IDLE_TIMEOUT.as_secs() as i64))));
}

/// After the owner changes their password: keep *this* session (new id, new
/// epoch) while every other session — issued under the old epoch — stops working.
pub async fn refresh_session_after_password_change(session: &Session, state: &WebState) {
    let _ = session.cycle_id().await;
    let _ = session.insert(SESSION_EPOCH, state.store.session_epoch()).await;
}

pub async fn is_authenticated(session: &Session, state: &WebState) -> bool {
    let auth = session.get::<bool>(AUTH_KEY).await.ok().flatten().unwrap_or(false);
    if !auth {
        return false;
    }
    let epoch = session.get::<u64>(SESSION_EPOCH).await.ok().flatten();
    let since = session.get::<i64>(SESSION_SINCE).await.ok().flatten();
    if session_is_valid(auth, epoch, since, state.store.session_epoch(), unix_now()) {
        true
    } else {
        // Stale (password changed / too old): destroy it instead of leaving it around.
        let _ = session.flush().await;
        false
    }
}

// ── handlers ──────────────────────────────────────────────────────────────────

pub async fn login_form(State(state): State<WebState>, session: Session) -> Response {
    // No admin yet → first-run wizard.
    if !state.store.is_initialized() {
        return Redirect::to("/welcome").into_response();
    }
    render_login(csrf_token(&session).await, None).into_response()
}

// ── First-run setup wizard (create the admin password — nothing logged) ───────

#[derive(Template)]
#[template(path = "welcome.html")]
struct WelcomeTemplate {
    csrf_token: String,
    error: Option<String>,
    /// Prefilled setup code (from the link printed in the startup log).
    code: String,
}

#[derive(Deserialize)]
pub struct WelcomeForm {
    csrf: String,
    #[serde(default)]
    code: String,
    password: String,
    confirm: String,
}

#[derive(Deserialize)]
pub struct WelcomeQuery {
    #[serde(default)]
    code: String,
}

pub async fn welcome_form(
    State(state): State<WebState>,
    session: Session,
    Query(q): Query<WelcomeQuery>,
) -> Response {
    if state.store.is_initialized() {
        return Redirect::to("/login").into_response();
    }
    render_welcome(csrf_token(&session).await, None, &q.code).into_response()
}

pub async fn welcome_submit(
    State(state): State<WebState>,
    session: Session,
    ClientIp(peer): ClientIp,
    Form(form): Form<WelcomeForm>,
) -> Response {
    if state.store.is_initialized() {
        return Redirect::to("/login").into_response();
    }
    let source = Source::from_peer(peer);
    let code = form.code.clone();
    if let Some(msg) = lockout_message(&state, source) {
        return render_welcome(csrf_token(&session).await, Some(msg), &code).into_response();
    }
    if !verify_csrf(&session, &form.csrf).await {
        return render_welcome(csrf_token(&session).await, Some("Invalid session — retry.".into()), &code)
            .into_response();
    }
    // The one-time setup code proves the visitor can read the bot's log / data
    // folder — i.e. is the owner — regardless of which address they came from.
    if !state.store.verify_setup_code(&form.code) {
        state.limiter.fail(source);
        return render_welcome(
            csrf_token(&session).await,
            Some("Setup code is incorrect. It's printed in the bot's startup log (and saved as setup.code in its data folder).".into()),
            &code,
        )
        .into_response();
    }
    if let Err(msg) = validate_new_password(&form.password) {
        return render_welcome(csrf_token(&session).await, Some(msg), &code).into_response();
    }
    if form.password != form.confirm {
        return render_welcome(csrf_token(&session).await, Some("Passwords do not match.".into()), &code)
            .into_response();
    }
    if let Err(e) = set_password_blocking(&state, &form.password).await {
        return render_welcome(csrf_token(&session).await, Some(format!("Failed to save: {e}")), &code)
            .into_response();
    }
    state.limiter.success(source);
    // Log them straight in, then on to wallet setup.
    begin_session(&session, &state).await;
    Redirect::to("/setup").into_response()
}

fn render_welcome(csrf_token: String, error: Option<String>, code: &str) -> Html<String> {
    let tpl = WelcomeTemplate { csrf_token, error, code: code.to_string() };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

pub async fn login_submit(
    State(state): State<WebState>,
    session: Session,
    ClientIp(peer): ClientIp,
    Form(form): Form<LoginForm>,
) -> Response {
    let source = Source::from_peer(peer);
    if let Some(msg) = lockout_message(&state, source) {
        return render_login(csrf_token(&session).await, Some(msg)).into_response();
    }

    if !verify_csrf(&session, &form.csrf).await {
        return render_login(csrf_token(&session).await, Some("Invalid session — retry.".into()))
            .into_response();
    }

    let ok = verify_password_blocking(&state, &form.password).await;
    if ok {
        state.limiter.success(source);
        // Rotates the id (session fixation), stamps epoch + login time, sets the idle timeout.
        begin_session(&session, &state).await;

        // Send to wallet setup if not configured yet, else to the dashboard.
        let dest = if !state.store.has_wallet() { "/setup" } else { "/" };
        Redirect::to(dest).into_response()
    } else {
        state.limiter.fail(source);
        render_login(csrf_token(&session).await, Some("Incorrect password.".into()))
            .into_response()
    }
}

#[derive(Deserialize)]
pub struct LogoutForm {
    #[serde(default)]
    csrf: String,
}

/// Signing out is a state change like any other: without the token a hostile page
/// could silently sign the owner out (a nuisance that can mask other attacks).
pub async fn logout(session: Session, Form(form): Form<LogoutForm>) -> Redirect {
    if !verify_csrf(&session, &form.csrf).await {
        return Redirect::to("/");
    }
    let _ = session.flush().await;
    Redirect::to("/login")
}

/// Route guard: first-run → /welcome, unauthenticated → /login, else proceed.
pub async fn require_auth(
    State(state): State<WebState>,
    session: Session,
    req: Request,
    next: Next,
) -> Response {
    if !state.store.is_initialized() {
        return Redirect::to("/welcome").into_response();
    }
    if is_authenticated(&session, &state).await {
        next.run(req).await
    } else {
        Redirect::to("/login").into_response()
    }
}

fn render_login(csrf_token: String, error: Option<String>) -> Html<String> {
    let tpl = LoginTemplate { csrf_token, error };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 3600;

    #[test]
    fn session_validity_rule() {
        let now = 1_000_000_000;
        // logged in, current epoch, young → valid
        assert!(session_is_valid(true, Some(3), Some(now - 100), 3, now));
        // not logged in
        assert!(!session_is_valid(false, Some(3), Some(now - 100), 3, now));
        // password changed since the session was issued
        assert!(!session_is_valid(true, Some(2), Some(now - 100), 3, now));
        // sessions from before epochs existed (no epoch / no login time) are rejected
        assert!(!session_is_valid(true, None, Some(now - 100), 3, now));
        assert!(!session_is_valid(true, Some(3), None, 3, now));
        // absolute maximum age: 7 days is the last valid moment, 7 days + 1s is not
        assert!(session_is_valid(true, Some(3), Some(now - 7 * DAY), 3, now));
        assert!(!session_is_valid(true, Some(3), Some(now - 7 * DAY - 1), 3, now));
        // a login time in the future (clock jump) is not trusted
        assert!(!session_is_valid(true, Some(3), Some(now + 60), 3, now));
    }
}
