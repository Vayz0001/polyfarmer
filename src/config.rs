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
    /// Dashboard bind address (default localhost; override for tunnels/containers).
    pub dashboard_bind: String,
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
        let dashboard_bind =
            env::var("DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

        Ok(Config { data_dir, markets_file, alerts_file, dashboard_bind })
    }
}
