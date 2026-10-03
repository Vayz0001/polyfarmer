//! Shared web state: credential store, live engine state, caches for slow
//! upstream reads, the live order-book hub, and a tiny login rate-limiter.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rust_decimal::Decimal;
use tokio::sync::{broadcast, mpsc, Notify, RwLock};

use crate::cache::TtlCache;
use crate::creds::CredentialStore;
use crate::engine::executor::Executor;
use crate::engine::ws_manager::AppState;
use crate::rewards::portfolio::{Activity, Position};
use crate::types::{Alert, WsCommand};

use super::book_hub::BookHub;

/// Lock the dashboard after this many consecutive failed logins.
pub const MAX_LOGIN_FAILS: u32 = 5;
/// Lockout duration once the fail threshold is hit.
pub const LOCKOUT_SECS: u64 = 30;

/// Freshness budgets for dashboard-only reads. None of these drive trading.
const SCORING_TTL: Duration = Duration::from_secs(15);
const BALANCE_TTL: Duration = Duration::from_secs(15);
const PORTFOLIO_TTL: Duration = Duration::from_secs(15);
const REWARDS_TTL: Duration = Duration::from_secs(60);
const PAYOUTS_TTL: Duration = Duration::from_secs(300);

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
    /// the `/events` SSE endpoint subscribes to push them to the UI.
    pub alert_tx: broadcast::Sender<Alert>,
    /// Pinged (by `events::spawn_state_watcher`) whenever engine state the UI
    /// shows changes — markets, order statuses, phase, connection health.
    pub state_tx: broadcast::Sender<()>,
    /// Path to `alerts.json` — read for the Activity feed's history on load.
    pub alerts_file: PathBuf,
    /// Poke the engine's quote loop to re-evaluate immediately instead of
    /// waiting for the next price event / 30s timer (e.g. right after a resume).
    pub quote_nudge: Arc<Notify>,
    /// The Polymarket wallet (proxy / deposit wallet) address — public, used
    /// for read-only Data API lookups (positions, value, fills). Set by the
    /// boot task once a wallet is configured.
    pub proxy_wallet: Arc<std::sync::RwLock<Option<String>>>,
    /// Caches for slow reads, so pages and the status strip never block on them.
    pub caches: Arc<Caches>,
    /// Live order books for the market view (its own WS connection — never the
    /// engine's, whose disconnect handling cancels live orders).
    pub book_hub: BookHub,
    /// Mark the session cookie `Secure` (see `Config::secure_cookies`).
    pub secure_cookies: bool,
}

pub struct Caches {
    /// `are_orders_scoring` results, keyed by the sorted order-id list.
    pub scoring: Arc<TtlCache<HashMap<String, bool>>>,
    /// pUSD collateral balance (single key).
    pub balance: Arc<TtlCache<Decimal>>,
    pub positions: Arc<TtlCache<Vec<Position>>>,
    pub value: Arc<TtlCache<Decimal>>,
    pub activity: Arc<TtlCache<Vec<Activity>>>,
    /// Today's accrued reward total (single key). Polymarket updates it slowly.
    pub rewards_today: Arc<TtlCache<Decimal>>,
    /// Full reward payout history (one payout a day — slow-moving).
    pub reward_payouts: Arc<TtlCache<Vec<Activity>>>,
}

impl Default for Caches {
    fn default() -> Self {
        Self {
            scoring: Arc::new(TtlCache::new(SCORING_TTL)),
            balance: Arc::new(TtlCache::new(BALANCE_TTL)),
            positions: Arc::new(TtlCache::new(PORTFOLIO_TTL)),
            value: Arc::new(TtlCache::new(PORTFOLIO_TTL)),
            activity: Arc::new(TtlCache::new(PORTFOLIO_TTL)),
            rewards_today: Arc::new(TtlCache::new(REWARDS_TTL)),
            reward_payouts: Arc::new(TtlCache::new(PAYOUTS_TTL)),
        }
    }
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
        let (state_tx, _) = broadcast::channel(16);
        Self {
            store,
            engine,
            login_guard: Arc::new(Mutex::new(LoginGuard::default())),
            polygon_rpc_url,
            wallet_ready: Arc::new(Notify::new()),
            engine_handle: EngineHandle::new(),
            reward_history_file,
            alert_tx,
            state_tx,
            alerts_file,
            quote_nudge,
            proxy_wallet: Arc::new(std::sync::RwLock::new(None)),
            caches: Arc::new(Caches::default()),
            book_hub: BookHub::new(),
            secure_cookies: false,
        }
    }

    /// Set the `Secure` attribute on the session cookie (HTTPS-only deployments).
    pub fn with_secure_cookies(mut self, secure: bool) -> Self {
        self.secure_cookies = secure;
        self
    }

    /// The configured Polymarket wallet address, if known.
    pub fn wallet_address(&self) -> Option<String> {
        self.proxy_wallet.read().unwrap().clone()
    }

    pub fn set_wallet_address(&self, addr: Option<String>) {
        *self.proxy_wallet.write().unwrap() = addr;
    }
}
