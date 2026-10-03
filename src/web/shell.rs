//! The app shell every page shares (`base.html`): nav state, CSRF token for
//! shell-level actions, and the live status strip (engine phase, market WS,
//! heartbeat, live orders, pUSD balance).

use std::time::Duration;

use askama::Template;
use axum::extract::State;
use axum::response::Html;
use tower_sessions::Session;

use crate::types::{EnginePhase, OrderStatus};

use super::auth::csrf_token;
use super::state::WebState;

/// Data `base.html` reads from every page template (`shell.*`).
pub struct Shell {
    /// Which nav item is highlighted: "overview" | "markets" | "positions" |
    /// "rewards" | "activity" | "settings".
    pub active: &'static str,
    /// Per-session CSRF token, for the shell-level forms (logout).
    pub csrf: String,
}

/// Every full page builds its shell here (this also mints the session's CSRF
/// token, so htmx actions on the page always have one to send).
pub async fn shell(session: &Session, active: &'static str) -> Shell {
    Shell { active, csrf: csrf_token(session).await }
}

/// Render any Askama template, surfacing template errors inline (they're
/// compile-checked, so this only fires on runtime formatting errors).
pub fn render<T: Template>(tpl: &T) -> Html<String> {
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

/// The engine's real health, derived from `AppState` — never from "is a
/// wallet file present".
pub struct EngineStatus {
    pub running: bool,
    pub phase_label: &'static str,
    /// "ok" | "warn" | "err" | "idle" — drives the dot colour.
    pub phase_tone: &'static str,
    pub ws_label: String,
    pub ws_tone: &'static str,
    pub hb_label: String,
    pub hb_tone: &'static str,
    pub live_orders: usize,
    pub markets: usize,
    pub paused_legs: usize,
}

/// A WS message within this long counts as "live" (the engine PINGs every 10s).
const WS_QUIET_AFTER: Duration = Duration::from_secs(30);
/// Heartbeats run every 5s; flag one that's overdue.
const HEARTBEAT_LATE_AFTER: Duration = Duration::from_secs(15);

pub async fn engine_status(state: &WebState) -> EngineStatus {
    let s = state.engine.read().await;
    let running = s.engine_phase == EnginePhase::Running;
    let (phase_label, phase_tone) = match s.engine_phase {
        EnginePhase::AwaitingWallet => ("No wallet", "idle"),
        EnginePhase::Starting => ("Starting", "warn"),
        EnginePhase::Running if s.heartbeat_paused => ("Paused · heartbeat", "err"),
        EnginePhase::Running => ("Running", "ok"),
        EnginePhase::Error => ("Start failed", "err"),
    };
    let (ws_label, ws_tone) = if !running {
        ("—".to_string(), "idle")
    } else if s.configs.is_empty() {
        ("Idle · no markets".to_string(), "idle")
    } else if s.ws_connected {
        match s.last_ws_msg.map(|t| t.elapsed()) {
            Some(age) if age < WS_QUIET_AFTER => ("Live".to_string(), "ok"),
            Some(age) => (format!("Quiet {}s", age.as_secs()), "warn"),
            None => ("Connected".to_string(), "ok"),
        }
    } else {
        ("Reconnecting".to_string(), "err")
    };
    let (hb_label, hb_tone) = if !running {
        ("—".to_string(), "idle")
    } else if s.heartbeat_paused {
        ("Failing".to_string(), "err")
    } else {
        match s.last_heartbeat_ok.map(|t| t.elapsed()) {
            Some(age) if age < HEARTBEAT_LATE_AFTER => ("OK".to_string(), "ok"),
            Some(age) => (format!("Late {}s", age.as_secs()), "warn"),
            None => ("Waiting".to_string(), "warn"),
        }
    };
    let live_orders = s.order_status.values().filter(|st| matches!(st, OrderStatus::Live { .. })).count();
    let markets = {
        let mut cids: Vec<&str> = s.configs.iter().map(|c| c.condition_id.as_str()).collect();
        cids.sort_unstable();
        cids.dedup();
        cids.len()
    };
    let paused_legs = s.configs.iter().filter(|c| c.paused).count();
    EngineStatus {
        running,
        phase_label,
        phase_tone,
        ws_label,
        ws_tone,
        hb_label,
        hb_tone,
        live_orders,
        markets,
        paused_legs,
    }
}

/// pUSD balance of the **Polymarket wallet** (the funder address you trade
/// from — not the signer/EOA key's own address), stale-while-revalidate so it
/// never blocks a render. Read on-chain (`balanceOf`); the CLOB's cached view is
/// only a fallback if every RPC fails. `None` until the first read lands.
pub async fn balance_swr(state: &WebState) -> Option<rust_decimal::Decimal> {
    let wallet: alloy::primitives::Address = state.wallet_address()?.parse().ok()?;
    let rpc = state.polygon_rpc_url.clone();
    let executor = state.engine_handle.get().await.map(|(e, _)| e);
    state.caches.balance.get_swr("balance", move || async move {
        match crate::wallet_detect::pusd_balance(wallet, rpc.as_deref()).await {
            Ok(v) => Ok(v),
            Err(e) => match executor {
                Some(ex) => tokio::time::timeout(Duration::from_secs(8), ex.collateral_balance())
                    .await
                    .map_err(|_| eyre::eyre!("timeout"))
                    .and_then(|r| r),
                None => Err(e),
            },
        }
    })
}

#[derive(Template)]
#[template(path = "_status_strip.html")]
struct StatusStripTemplate {
    st: EngineStatus,
    balance: Option<String>,
}

/// GET /status/strip — the shell's live status bar (refreshed on state events).
pub async fn status_strip(State(state): State<WebState>) -> Html<String> {
    let st = engine_status(&state).await;
    let balance = balance_swr(&state).await.map(|b| format!("${b:.2}"));
    render(&StatusStripTemplate { st, balance })
}
