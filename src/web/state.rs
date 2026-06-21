//! Shared web state: credential store, live engine state, and a tiny in-memory
//! login rate-limiter.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{Notify, RwLock};

use crate::creds::CredentialStore;
use crate::engine::ws_manager::AppState;

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
        Self {
            store,
            engine,
            login_guard: Arc::new(Mutex::new(LoginGuard::default())),
            polygon_rpc_url,
            wallet_ready: Arc::new(Notify::new()),
        }
    }
}
