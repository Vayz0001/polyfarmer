//! Settings (also the first-run wallet step): enter the wallet credentials
//! (validated, then encrypted to the store), change the admin password, and
//! client-side display preferences.

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use askama::Template;
use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use tower_sessions::Session;
use zeroize::Zeroizing;

use super::auth::{
    csrf_token, refresh_session_after_password_change, set_password_blocking, verify_csrf, verify_password_blocking,
    AttemptSource, ClientIp,
};
use super::shell::{render as render_tpl, shell, Shell};
use super::state::WebState;
use crate::types::EnginePhase;

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    shell: Shell,
    csrf_token: String,
    has_wallet: bool,
    /// Configured Polymarket wallet address (public), for display.
    wallet_addr: Option<String>,
    notice: Option<String>,
    error: Option<String>,
    pw_notice: Option<String>,
    pw_error: Option<String>,
}

#[derive(Deserialize)]
pub struct PasswordForm {
    csrf: String,
    #[serde(default)]
    current: String,
    password: String,
    confirm: String,
}

#[derive(Deserialize)]
pub struct WalletForm {
    csrf: String,
    /// Wiped from memory when the form is dropped, on every exit path. (The raw
    /// request body buffer is out of our reach, so this is best effort.)
    private_key: Zeroizing<String>,
    proxy_wallet: String,
}

#[derive(Deserialize)]
pub struct DetectForm {
    csrf: String,
    private_key: Zeroizing<String>,
}

#[derive(Template)]
#[template(path = "_wallet_save_result.html")]
struct WalletSaveResultTemplate {
    saved: bool,
    message: String,
}

#[derive(Template)]
#[template(path = "_engine_status.html")]
struct EngineStatusTemplate {
    failed: bool,
}

#[derive(Template)]
#[template(path = "launching.html")]
struct LaunchingTemplate {}

#[derive(Template)]
#[template(path = "_wallet_detect.html")]
struct WalletDetectTemplate {
    /// Whether to reveal the (always-editable) address field at all — stays
    /// hidden until the private key has been validated, so there's nothing
    /// to see (or mistakenly edit) before that.
    reveal: bool,
    /// Pre-filled value for the address field once revealed (empty for
    /// manual entry, the detected address when confirmed).
    value: String,
    message: String,
    verified: bool,
    candidates: Vec<String>,
}

pub async fn page(State(state): State<WebState>, session: Session) -> Html<String> {
    render(&state, &session, None, None).await
}

async fn render_pw(state: &WebState, session: &Session, notice: Option<String>, error: Option<String>) -> Response {
    render_full(state, session, None, None, notice, error).await.into_response()
}

pub async fn set_password(
    State(state): State<WebState>,
    session: Session,
    ClientIp(peer): ClientIp,
    Form(form): Form<PasswordForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return render_pw(&state, &session, None, Some("Invalid session — retry.".into())).await;
    }
    // Re-authentication is throttled like login: a hijacked session must not get
    // unlimited guesses at the current password.
    let source = AttemptSource::from_peer(peer);
    if let Some(wait) = state.limiter.check(source) {
        let msg = format!("Too many attempts — try again in {} seconds.", wait.as_secs().max(1));
        return render_pw(&state, &session, None, Some(msg)).await;
    }
    if !verify_password_blocking(&state, &form.current).await {
        state.limiter.fail(source);
        return render_pw(&state, &session, None, Some("Current password is incorrect.".into())).await;
    }
    state.limiter.success(source);
    if let Err(msg) = crate::creds::validate_new_password(&form.password) {
        return render_pw(&state, &session, None, Some(msg)).await;
    }
    if form.password != form.confirm {
        return render_pw(&state, &session, None, Some("New passwords do not match.".into())).await;
    }
    match set_password_blocking(&state, &form.password).await {
        Ok(_) => {
            // Every other session (issued under the old password) is now invalid;
            // keep this one, with a fresh id.
            refresh_session_after_password_change(&session, &state).await;
            render_pw(
                &state,
                &session,
                Some("Password changed. Other signed-in sessions were signed out.".into()),
                None,
            )
            .await
        }
        Err(e) => render_pw(&state, &session, None, Some(format!("Failed to save: {e}"))).await,
    }
}

pub async fn set_wallet(
    State(state): State<WebState>,
    session: Session,
    headers: HeaderMap,
    Form(form): Form<WalletForm>,
) -> Response {
    // htmx submits get a small inline fragment (button → "✓ Saved", no reload);
    // a plain form post (JS disabled) falls back to a full-page re-render.
    let htmx = headers.contains_key("HX-Request");

    // Is the engine still parked in its boot wait-loop? If so, saving a wallet
    // auto-starts it (no restart). If it's already Running, this is a wallet
    // *change*, which still needs a restart to switch safely.
    let first_time = {
        let phase = state.engine.read().await.engine_phase.clone();
        matches!(phase, EnginePhase::AwaitingWallet | EnginePhase::Error)
    };

    if !verify_csrf(&session, &form.csrf).await {
        return save_result(&state, &session, htmx, false, "Invalid session — retry.".into()).await;
    }
    // Format-only validation — typos are still caught immediately. We do NOT
    // cross-check the address against a locally-derived CREATE2 address:
    // Polymarket wallets can be deployed via more than one proxy variant/
    // factory, and verified on-chain that a real wallet's bytecode can embed
    // the correct EOA while matching neither of our derivable candidates. A
    // hard block on that basis risks rejecting valid wallets, which is worse
    // than not checking at all.
    let key = form.private_key.trim();
    let wallet = form.proxy_wallet.trim();

    if key.parse::<PrivateKeySigner>().is_err() {
        let hex_len = key.trim_start_matches("0x").trim_start_matches("0X").len();
        let hint = if hex_len == 40 {
            " That's 40 hex characters — the length of a wallet address. \
             A private key is 64 hex characters (32 bytes); it's a different value."
        } else {
            ""
        };
        return save_result(&state, &session, htmx, false, format!("Private key is not valid.{hint}")).await;
    }
    if wallet.parse::<Address>().is_err() {
        return save_result(&state, &session, htmx, false, "Wallet address is not valid.".into()).await;
    }

    match state.store.set_wallet(key, wallet) {
        Ok(_) => {
            if first_time {
                // Only show the new address once the engine is (re)starting on
                // it. While an engine is Running on the old wallet, the
                // dashboard must keep describing that wallet until a restart.
                state.set_wallet_address(Some(wallet.to_string()));
                // Wake the parked boot task so it starts the engine now, and
                // hand off to the launch screen, which polls the engine up and
                // flows straight into the dashboard — no restart.
                state.wallet_ready.notify_one();
                return if htmx { hx_redirect("/launching") } else { Redirect::to("/launching").into_response() };
            }
            // Wallet change on an already-running engine — switching trading
            // wallets safely needs a restart (avoids orphaning open orders).
            save_result(&state, &session, htmx, true, "Wallet saved (encrypted).".into()).await
        }
        Err(e) => save_result(&state, &session, htmx, false, format!("Failed to save: {e}")).await,
    }
}

/// Polled by the launch screen. On `Running` it returns an `HX-Redirect` so the
/// browser flows into the dashboard; on `Error` it reveals an inline failure;
/// otherwise it stays an invisible re-arming poller.
pub async fn engine_status(State(state): State<WebState>) -> Response {
    let phase = state.engine.read().await.engine_phase.clone();
    if phase == EnginePhase::Running {
        return hx_redirect("/");
    }
    let tpl = EngineStatusTemplate { failed: phase == EnginePhase::Error };
    Html(tpl.render().unwrap_or_else(|e| super::shell::render_failed(&e))).into_response()
}

/// The first-run launch screen — a calm branded interstitial that polls the
/// engine up and redirects into the dashboard. Visiting it without a wallet
/// (e.g. directly) just bounces back to setup.
pub async fn launching(State(state): State<WebState>) -> Response {
    if !state.store.has_wallet() {
        return Redirect::to("/setup").into_response();
    }
    let tpl = LaunchingTemplate {};
    Html(tpl.render().unwrap_or_else(|e| super::shell::render_failed(&e))).into_response()
}

/// Empty 200 carrying htmx's `HX-Redirect` header → client-side navigation.
fn hx_redirect(to: &'static str) -> Response {
    let mut resp = Html(String::new()).into_response();
    resp.headers_mut().insert("HX-Redirect", HeaderValue::from_static(to));
    resp
}

/// Renders the outcome of a wallet *change* (or an error) for the setup form.
/// First-run saves never reach here — they redirect to the launch screen.
async fn save_result(state: &WebState, session: &Session, htmx: bool, saved: bool, message: String) -> Response {
    if htmx {
        let tpl = WalletSaveResultTemplate { saved, message: if saved { String::new() } else { message } };
        return Html(tpl.render().unwrap_or_else(|e| super::shell::render_failed(&e))).into_response();
    }
    // Non-htmx fallback (JS disabled) — full-page re-render with the notice.
    if saved {
        render(state, session, Some(format!("{message} Restart the bot to switch wallets.")), None)
            .await
            .into_response()
    } else {
        render(state, session, None, Some(message)).await.into_response()
    }
}

/// HTMX-triggered as the user types their private key: looks up which
/// Polymarket wallet(s) actually exist on-chain for the derived EOA and
/// returns a small fragment that either auto-fills the address field (one
/// confident match), offers a pick-list (multiple matches — can legitimately
/// happen, e.g. an old Safe/Proxy wallet alongside a newer Deposit Wallet),
/// or says nothing found (fresh key — fall back to manual entry).
///
/// Only the derived *public* EOA address is ever sent to the RPC; the key
/// itself never leaves this request to our own server.
pub async fn detect_wallet(
    State(state): State<WebState>,
    session: Session,
    Form(form): Form<DetectForm>,
) -> Html<String> {
    // CSRF failure — say nothing, reset to hidden.
    if !verify_csrf(&session, &form.csrf).await {
        return render_detect(false, "", String::new(), false, Vec::new());
    }

    // The client-side format check (see setup.html) already gates the htmx
    // request to plausible-looking keys and gives instant feedback for
    // anything else — this is just a defensive fallback (e.g. JS disabled).
    let signer: PrivateKeySigner = match form.private_key.trim().parse() {
        Ok(s) => s,
        Err(_) => return render_detect(false, "", "Invalid private key".to_string(), false, Vec::new()),
    };

    match crate::wallet_detect::detect_wallets(signer.address(), state.polygon_rpc_url.as_deref()).await {
        Ok(candidates) if candidates.len() == 1 => {
            let c = &candidates[0];
            render_detect(true, &c.address.to_string(), String::new(), true, Vec::new())
        }
        Ok(candidates) if candidates.len() > 1 => render_detect(
            true,
            "",
            "Found a few wallets for this key. Pick the one you use on Polymarket:".into(),
            false,
            candidates.iter().map(|c| c.address.to_string()).collect(),
        ),
        Ok(_) => {
            render_detect(true, "", "Key looks good. Enter your Polymarket address below.".into(), false, Vec::new())
        }
        Err(_) => render_detect(
            true,
            "",
            "Couldn't verify automatically. Enter your Polymarket address below.".into(),
            false,
            Vec::new(),
        ),
    }
}

fn render_detect(reveal: bool, value: &str, message: String, verified: bool, candidates: Vec<String>) -> Html<String> {
    let tpl = WalletDetectTemplate { reveal, value: value.to_string(), message, verified, candidates };
    Html(tpl.render().unwrap_or_else(|e| super::shell::render_failed(&e)))
}

async fn render(state: &WebState, session: &Session, notice: Option<String>, error: Option<String>) -> Html<String> {
    render_full(state, session, notice, error, None, None).await
}

async fn render_full(
    state: &WebState,
    session: &Session,
    notice: Option<String>,
    error: Option<String>,
    pw_notice: Option<String>,
    pw_error: Option<String>,
) -> Html<String> {
    let wallet_addr = state.wallet_address().or_else(|| {
        // Before the engine has booted, read the (public) address from the store.
        state.store.load_wallet().ok().flatten().map(|w| w.proxy_wallet)
    });
    render_tpl(&SetupTemplate {
        shell: shell(session, "settings").await,
        csrf_token: csrf_token(session).await,
        has_wallet: state.store.has_wallet(),
        wallet_addr,
        notice,
        error,
        pw_notice,
        pw_error,
    })
}
