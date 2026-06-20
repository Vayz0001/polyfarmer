use alloy::primitives::Address;
use eyre::{eyre, Result};
use secrecy::SecretString;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

pub struct Config {
    /// EOA private key — kept in SecretString to prevent accidental logging
    pub private_key: SecretString,

    /// Gnosis Safe proxy wallet address (the one shown on polymarket.com/settings)
    pub proxy_wallet: Address,

    /// Path to markets.json (written by TS Discord bot, polled by Rust)
    pub markets_file: PathBuf,

    /// Path to alerts.json (appended by Rust, read+cleared by TS Discord bot)
    pub alerts_file: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let private_key = SecretString::from(
            env::var("POLYMARKET_PRIVATE_KEY")
                .map_err(|_| eyre!("POLYMARKET_PRIVATE_KEY not set"))?,
        );

        let proxy_wallet = Address::from_str(
            &env::var("POLYMARKET_PROXY_WALLET")
                .map_err(|_| eyre!("POLYMARKET_PROXY_WALLET not set"))?,
        )
        .map_err(|_| eyre!("Invalid POLYMARKET_PROXY_WALLET address"))?;

        let markets_file = PathBuf::from(
            env::var("MARKETS_FILE").unwrap_or_else(|_| "markets.json".to_string()),
        );

        let alerts_file = PathBuf::from(
            env::var("ALERTS_FILE").unwrap_or_else(|_| "alerts.json".to_string()),
        );

        Ok(Config { private_key, proxy_wallet, markets_file, alerts_file })
    }
}
