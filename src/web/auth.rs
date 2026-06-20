//! Authentication: session login/logout, the route guard, CSRF, and a basic
//! login rate-limiter. Sessions are server-side (tower-sessions); the cookie is
//! HttpOnly + SameSite=Lax so an exposed/tunnelled dashboard stays safe.

use std::time::{Duration, Instant};

use askama::Template;
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::Deserialize;
use tower_sessions::Session;

use super::state::{WebState, LOCKOUT_SECS, MAX_LOGIN_FAILS};

const SESSION_AUTH: &str = "auth";
const SESSION_CSRF: &str = "csrf";

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

// ── auth state helpers ────────────────────────────────────────────────────────

pub async fn is_authenticated(session: &Session) -> bool {
    session.get::<bool>(SESSION_AUTH).await.ok().flatten().unwrap_or(false)
}

// ── handlers ──────────────────────────────────────────────────────────────────

pub async fn login_form(session: Session) -> Html<String> {
    // Already logged in? bounce home.
    let token = csrf_token(&session).await;
    render_login(token, None)
}

pub async fn login_submit(
    State(state): State<WebState>,
    session: Session,
    Form(form): Form<LoginForm>,
) -> Response {
    // Rate-limit check — never hold the std Mutex guard across an `.await`
    // (that would make the handler future `!Send`).
    let locked = {
        let mut guard = state.login_guard.lock().unwrap();
        match guard.locked_until {
            Some(until) if Instant::now() < until => true,
            _ => {
                guard.locked_until = None;
                guard.fails = 0;
                false
            }
        }
    };
    if locked {
        return render_login(
            csrf_token(&session).await,
            Some("Too many attempts — try again shortly.".into()),
        )
        .into_response();
    }

    if !verify_csrf(&session, &form.csrf).await {
        return render_login(csrf_token(&session).await, Some("Invalid session — retry.".into()))
            .into_response();
    }

    let ok = state.store.verify_login(&form.password).unwrap_or(false);
    if ok {
        {
            let mut guard = state.login_guard.lock().unwrap();
            guard.fails = 0;
            guard.locked_until = None;
        }
        // Prevent session fixation: rotate the id, then mark authenticated.
        let _ = session.cycle_id().await;
        let _ = session.insert(SESSION_AUTH, true).await;

        // Force password change on the auto-generated first-run password.
        let must_change = state.store.must_change_password().unwrap_or(false);
        let dest = if must_change || !state.store.has_wallet() { "/setup" } else { "/" };
        Redirect::to(dest).into_response()
    } else {
        // Scope the guard so it is dropped before the `.await` below.
        {
            let mut guard = state.login_guard.lock().unwrap();
            guard.fails += 1;
            if guard.fails >= MAX_LOGIN_FAILS {
                guard.locked_until = Some(Instant::now() + Duration::from_secs(LOCKOUT_SECS));
                guard.fails = 0;
            }
        }
        render_login(csrf_token(&session).await, Some("Incorrect password.".into()))
            .into_response()
    }
}

pub async fn logout(session: Session) -> Redirect {
    let _ = session.flush().await;
    Redirect::to("/login")
}

/// Route guard: redirect unauthenticated requests to /login.
pub async fn require_auth(session: Session, req: Request, next: Next) -> Response {
    if is_authenticated(&session).await {
        next.run(req).await
    } else {
        Redirect::to("/login").into_response()
    }
}

fn render_login(csrf_token: String, error: Option<String>) -> Html<String> {
    let tpl = LoginTemplate { csrf_token, error };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}
