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
    /// alerts.json — append-only alert log (shown in the dashboard's Activity feed).
    pub alerts_file: PathBuf,
    /// reward_history.json — daily reward-earnings snapshots (no Polymarket
    /// history endpoint exists, so we build our own).
    pub reward_history_file: PathBuf,
    /// Dashboard bind address (default localhost; override for tunnels/containers).
    pub dashboard_bind: String,
    /// Mark the session cookie `Secure` (HTTPS only). Turn on when the
    /// dashboard is served over HTTPS (e.g. `tailscale serve`, Caddy); leave
    /// off for plain `http://localhost` / `http://<tailscale-ip>`, where
    /// browsers would drop a Secure cookie and login would silently fail.
    pub secure_cookies: bool,
    /// Optional custom Polygon RPC for on-chain wallet detection during setup.
    /// Falls back to a public-RPC list if unset — see `wallet_detect`.
    pub polygon_rpc_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let data_dir = PathBuf::from(env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string()));
        let markets_file = PathBuf::from(env::var("MARKETS_FILE").unwrap_or_else(|_| "data/markets.json".to_string()));
        let alerts_file = PathBuf::from(env::var("ALERTS_FILE").unwrap_or_else(|_| "data/alerts.json".to_string()));
        let reward_history_file =
            PathBuf::from(env::var("REWARD_HISTORY_FILE").unwrap_or_else(|_| "data/reward_history.json".to_string()));
        let dashboard_bind = env::var("DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
        let secure_cookies = env::var("DASHBOARD_SECURE_COOKIES").map(|v| is_truthy(&v)).unwrap_or(false);
        let polygon_rpc_url = env::var("POLYGON_RPC_URL").ok();

        Ok(Config {
            data_dir,
            markets_file,
            alerts_file,
            reward_history_file,
            dashboard_bind,
            secure_cookies,
            polygon_rpc_url,
        })
    }
}

/// `1` / `true` / `yes` / `on` (any case).
fn is_truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Whether a `host:port` bind address only accepts connections from this machine.
pub fn bind_is_loopback(bind: &str) -> bool {
    if let Ok(addr) = bind.parse::<std::net::SocketAddr>() {
        return addr.ip().is_loopback();
    }
    // Hostname form, e.g. `localhost:8080`.
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    host.eq_ignore_ascii_case("localhost")
}

/// Startup advice about how the dashboard is exposed (empty = nothing to say).
pub fn exposure_warnings(bind: &str, secure_cookies: bool) -> Vec<String> {
    let mut out = Vec::new();
    if !bind_is_loopback(bind) {
        out.push(format!(
            "Dashboard is bound to {bind}, which other machines can reach. Don't expose it to the public internet — \
             keep it on 127.0.0.1 and reach it through Tailscale (`tailscale serve`), an SSH tunnel or an HTTPS reverse proxy."
        ));
        if !secure_cookies {
            out.push(
                "Serving over HTTPS? Set DASHBOARD_SECURE_COOKIES=true so the session cookie is HTTPS-only."
                    .to_string(),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection() {
        for b in ["127.0.0.1:8080", "[::1]:8080", "localhost:8080", "LOCALHOST:1"] {
            assert!(bind_is_loopback(b), "{b}");
        }
        for b in ["0.0.0.0:8080", "[::]:8080", "100.64.1.2:8080", "192.168.1.5:8080", "example.com:80"] {
            assert!(!bind_is_loopback(b), "{b}");
        }
    }

    #[test]
    fn truthy_values() {
        for v in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(is_truthy(v), "{v}");
        }
        for v in ["", "0", "false", "no", "off", "maybe"] {
            assert!(!is_truthy(v), "{v}");
        }
    }

    #[test]
    fn warns_only_when_reachable_from_elsewhere() {
        assert!(exposure_warnings("127.0.0.1:8080", false).is_empty());
        let w = exposure_warnings("0.0.0.0:8080", false);
        assert_eq!(w.len(), 2);
        assert!(w[0].contains("Tailscale") && w[1].contains("DASHBOARD_SECURE_COOKIES"));
        // With secure cookies on, only the exposure warning remains.
        assert_eq!(exposure_warnings("0.0.0.0:8080", true).len(), 1);
    }
}
