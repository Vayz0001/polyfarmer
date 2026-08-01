//! Process configuration from the environment.
//!
//! Note: wallet **secrets** (private key, proxy wallet) are NOT here — they are
//! entered via the dashboard and stored encrypted by [`crate::creds`]. This file
//! only holds non-secret paths/bind settings, so the bot can boot (and serve the
//! setup dashboard) without any `.env`.

use eyre::Result;
use std::env;
use std::path::PathBuf;

pub struct Config {
    /// Directory for runtime state + encrypted credentials (gitignored).
    pub data_dir: PathBuf,
    /// markets.json — persisted market configs.
    pub markets_file: PathBuf,
    /// alerts.json — append-only alert log (read by the optional Discord notifier).
    pub alerts_file: PathBuf,
    /// reward_history.json — daily reward-earnings snapshots (no Polymarket
    /// history endpoint exists, so we build our own).
    pub reward_history_file: PathBuf,
    /// Dashboard bind address (default localhost; override for tunnels/containers).
    pub dashboard_bind: String,
    /// Optional custom Polygon RPC for on-chain wallet detection during setup.
    /// Falls back to a public-RPC list if unset — see `wallet_detect`.
    pub polygon_rpc_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let data_dir =
            PathBuf::from(env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string()));
        let markets_file = PathBuf::from(
            env::var("MARKETS_FILE").unwrap_or_else(|_| "data/markets.json".to_string()),
        );
        let alerts_file = PathBuf::from(
            env::var("ALERTS_FILE").unwrap_or_else(|_| "data/alerts.json".to_string()),
        );
        let reward_history_file = PathBuf::from(
            env::var("REWARD_HISTORY_FILE").unwrap_or_else(|_| "data/reward_history.json".to_string()),
        );
        let dashboard_bind =
            env::var("DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
        let polygon_rpc_url = env::var("POLYGON_RPC_URL").ok();

        Ok(Config {
            data_dir,
            markets_file,
            alerts_file,
            reward_history_file,
            dashboard_bind,
            polygon_rpc_url,
        })
    }
}
