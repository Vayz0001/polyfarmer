//! First-run / settings: change the admin password and enter the wallet
//! credentials (validated, then encrypted to the store).

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use askama::Template;
use axum::{
    extract::State,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use tower_sessions::Session;

use super::auth::{csrf_token, verify_csrf};
use super::state::WebState;

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    csrf_token: String,
    has_wallet: bool,
    notice: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct PasswordForm {
    csrf: String,
    password: String,
    confirm: String,
}

#[derive(Deserialize)]
pub struct WalletForm {
    csrf: String,
    private_key: String,
    proxy_wallet: String,
}

pub async fn page(State(state): State<WebState>, session: Session) -> Html<String> {
    render(&state, &session, None, None).await
}

pub async fn set_password(
    State(state): State<WebState>,
    session: Session,
    Form(form): Form<PasswordForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return render(&state, &session, None, Some("Invalid session — retry.".into()))
            .await
            .into_response();
    }
    if form.password.len() < 8 {
        return render(&state, &session, None, Some("Password must be at least 8 characters.".into()))
            .await
            .into_response();
    }
    if form.password != form.confirm {
        return render(&state, &session, None, Some("Passwords do not match.".into()))
            .await
            .into_response();
    }
    match state.store.set_password(&form.password) {
        Ok(_) => Redirect::to("/setup").into_response(),
        Err(e) => render(&state, &session, None, Some(format!("Failed to save: {e}")))
            .await
            .into_response(),
    }
}

pub async fn set_wallet(
    State(state): State<WebState>,
    session: Session,
    Form(form): Form<WalletForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return render(&state, &session, None, Some("Invalid session — retry.".into()))
            .await
            .into_response();
    }
    // Validate before encrypting so typos are caught immediately.
    let key = form.private_key.trim();
    let wallet = form.proxy_wallet.trim();
    if key.parse::<PrivateKeySigner>().is_err() {
        return render(&state, &session, None, Some("Private key is not valid.".into()))
            .await
            .into_response();
    }
    if wallet.parse::<Address>().is_err() {
        return render(&state, &session, None, Some("Wallet address is not valid.".into()))
            .await
            .into_response();
    }
    match state.store.set_wallet(key, wallet) {
        Ok(_) => render(
            &state,
            &session,
            Some("Wallet saved (encrypted). Restart the bot to begin trading.".into()),
            None,
        )
        .await
        .into_response(),
        Err(e) => render(&state, &session, None, Some(format!("Failed to save: {e}")))
            .await
            .into_response(),
    }
}

async fn render(
    state: &WebState,
    session: &Session,
    notice: Option<String>,
    error: Option<String>,
) -> Html<String> {
    let tpl = SetupTemplate {
        csrf_token: csrf_token(session).await,
        has_wallet: state.store.has_wallet(),
        notice,
        error,
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}
