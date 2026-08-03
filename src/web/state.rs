//! Shared web state: credential store, live engine state, and a tiny in-memory
//! login rate-limiter.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{broadcast, mpsc, Notify, RwLock};

use crate::creds::CredentialStore;
use crate::engine::executor::Executor;
use crate::engine::ws_manager::AppState;
use crate::types::{Alert, WsCommand};

/// Lock the dashboard after this many consecutive failed logins.
pub const MAX_LOGIN_FAILS: u32 = 5;
/// Lockout duration once the fail threshold is hit.
pub const LOCKOUT_SECS: u64 = 30;

#[derive(Clone)]
pub struct WebState {
    pub store: Arc<CredentialStore>,
    /// Live engine state — shared with the trading engine. Populated by the
    /// engine when running; readable by the dashboard at all times.
    pub engine: Arc<RwLock<AppState>>,
    pub login_guard: Arc<Mutex<LoginGuard>>,
    /// Optional custom Polygon RPC for on-chain wallet detection (see
    /// `wallet_detect`). `None` means use the public fallback list.
    pub polygon_rpc_url: Option<String>,
    /// Pinged by `set_wallet` so the boot task can start the engine the moment
    /// a wallet is first configured — no restart. Carries no data (the boot
    /// task re-reads the encrypted file itself), so no secret crosses it.
    pub wallet_ready: Arc<Notify>,
    /// Reaches the running engine's authenticated SDK client + WS command
    /// channel from request handlers (market add/remove/pause, reward reads).
    /// `None` until `EnginePhase::Running`.
    pub engine_handle: EngineHandle,
    /// Path to the self-built daily reward-history log (see `rewards::history`
    /// — Polymarket has no range/history endpoint, only single-day queries).
    pub reward_history_file: PathBuf,
    /// Live alert fan-out: the engine's `Alerter` publishes every alert here;
    /// the `/activity/stream` SSE endpoint subscribes to push them to the UI.
    pub alert_tx: broadcast::Sender<Alert>,
    /// Path to `alerts.json` — read for the Activity feed's history on load.
    pub alerts_file: PathBuf,
    /// Poke the engine's quote loop to re-evaluate immediately instead of
    /// waiting for the next price event / 30s timer (e.g. right after a resume).
    pub quote_nudge: Arc<Notify>,
}

/// Bundles the authenticated [`Executor`] and the WS command sender as one
/// unit — handlers always need both together (e.g. add-market both writes
/// `AppState` and may need to send `WsCommand::Subscribe`), and they only
/// exist from the same moment (set once, right after the engine boots).
#[derive(Clone, Default)]
pub struct EngineHandle {
    inner: Arc<RwLock<Option<EngineHandleInner>>>,
}

struct EngineHandleInner {
    executor: Arc<Executor>,
    ws_cmd_tx: mpsc::Sender<WsCommand>,
}

impl EngineHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called once by the boot task, right after the executor and WS channel
    /// exist (see `app.rs`).
    pub async fn set(&self, executor: Arc<Executor>, ws_cmd_tx: mpsc::Sender<WsCommand>) {
        *self.inner.write().await = Some(EngineHandleInner { executor, ws_cmd_tx });
    }

    /// Clones both handles out in one lock acquisition; the guard is dropped
    /// before returning, so callers never hold it across an `.await`.
    /// Returns `None` if the engine hasn't reached `Running` yet.
    pub async fn get(&self) -> Option<(Arc<Executor>, mpsc::Sender<WsCommand>)> {
        self.inner
            .read()
            .await
            .as_ref()
            .map(|h| (Arc::clone(&h.executor), h.ws_cmd_tx.clone()))
    }
}

#[derive(Default)]
pub struct LoginGuard {
    pub fails: u32,
    pub locked_until: Option<Instant>,
}

impl WebState {
    pub fn new(store: Arc<CredentialStore>, engine: Arc<RwLock<AppState>>) -> Self {
        Self::with_rpc(store, engine, None)
    }

    pub fn with_rpc(
        store: Arc<CredentialStore>,
        engine: Arc<RwLock<AppState>>,
        polygon_rpc_url: Option<String>,
    ) -> Self {
        let (alert_tx, _) = broadcast::channel(16);
        Self::with_config(
            store,
            engine,
            polygon_rpc_url,
            "data/reward_history.json".into(),
            alert_tx,
            "data/alerts.json".into(),
            Arc::new(Notify::new()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_config(
        store: Arc<CredentialStore>,
        engine: Arc<RwLock<AppState>>,
        polygon_rpc_url: Option<String>,
        reward_history_file: PathBuf,
        alert_tx: broadcast::Sender<Alert>,
        alerts_file: PathBuf,
        quote_nudge: Arc<Notify>,
    ) -> Self {
        Self {
            store,
            engine,
            login_guard: Arc::new(Mutex::new(LoginGuard::default())),
            polygon_rpc_url,
            wallet_ready: Arc::new(Notify::new()),
            engine_handle: EngineHandle::new(),
            reward_history_file,
            alert_tx,
            alerts_file,
            quote_nudge,
        }
    }
}
