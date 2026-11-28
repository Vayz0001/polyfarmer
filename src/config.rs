//! Process configuration from the environment.
//!
//! Note: wallet **secrets** (private key, proxy wallet) are NOT here — they are
//! entered via the dashboard and stored encrypted by [`crate::creds`]. This file
//! only holds non-secret paths/bind settings, so the bot can boot (and serve the
//! setup dashboard) without any `.env`.

use eyre::{Result, WrapErr};
use std::env;
use std::path::{Path, PathBuf};

pub struct Config {
    /// Directory for runtime state + encrypted credentials (gitignored).
    pub data_dir: PathBuf,
    /// markets.json — persisted market configs.
    pub markets_file: PathBuf,
    /// alerts.json — append-only alert log (shown in the dashboard's Activity feed).
    pub alerts_file: PathBuf,
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

        let vars = PathVars {
            data_dir: env::var("DATA_DIR").ok().map(PathBuf::from),
            markets_file: env::var("MARKETS_FILE").ok().map(PathBuf::from),
            alerts_file: env::var("ALERTS_FILE").ok().map(PathBuf::from),
        };
        let paths = data_paths(&vars);
        if let Some(warning) = files_left_behind(&vars, &paths, Path::new("./data"))? {
            tracing::warn!("{warning}");
        }
        let DataPaths { data_dir, markets_file, alerts_file } = paths;
        let dashboard_bind = env::var("DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
        let secure_cookies = env::var("DASHBOARD_SECURE_COOKIES").map(|v| is_truthy(&v)).unwrap_or(false);
        let polygon_rpc_url = env::var("POLYGON_RPC_URL").ok();

        Ok(Config { data_dir, markets_file, alerts_file, dashboard_bind, secure_cookies, polygon_rpc_url })
    }
}

/// The path settings as set in the environment (`None` = not set).
struct PathVars {
    data_dir: Option<PathBuf>,
    markets_file: Option<PathBuf>,
    alerts_file: Option<PathBuf>,
}

/// Where state is stored.
#[derive(Debug, PartialEq)]
struct DataPaths {
    data_dir: PathBuf,
    markets_file: PathBuf,
    alerts_file: PathBuf,
}

/// The paths `vars` set. Unset files default to inside the data folder
/// (`data` when DATA_DIR is unset too).
fn data_paths(vars: &PathVars) -> DataPaths {
    let data_dir = vars.data_dir.clone().unwrap_or_else(|| PathBuf::from("data"));
    DataPaths {
        markets_file: vars.markets_file.clone().unwrap_or_else(|| data_dir.join("markets.json")),
        alerts_file: vars.alerts_file.clone().unwrap_or_else(|| data_dir.join("alerts.json")),
        data_dir,
    }
}

/// Before DATA_DIR moved the default markets and alerts files, they stayed in
/// `legacy_dir` (./data) whatever DATA_DIR said. With DATA_DIR set, a file left
/// to its default that is still only there would quietly be dropped: for the
/// legs that is an error (nothing is moved or deleted), for the activity log,
/// which is only history, the returned warning.
fn files_left_behind(vars: &PathVars, paths: &DataPaths, legacy_dir: &Path) -> Result<Option<String>> {
    let Some(data_dir) = &vars.data_dir else { return Ok(None) };
    if vars.markets_file.is_none() {
        if let Some(old) = left_behind(&paths.markets_file, legacy_dir, "markets.json")? {
            eyre::bail!(
                "Your markets file is at {} but DATA_DIR is {}. Move it to {}, or set MARKETS_FILE to its path.",
                old.display(),
                data_dir.display(),
                paths.markets_file.display()
            );
        }
    }
    if vars.alerts_file.is_some() {
        return Ok(None);
    }
    Ok(match left_behind(&paths.alerts_file, legacy_dir, "alerts.json") {
        Ok(None) => None,
        Ok(Some(old)) => Some(format!(
            "Your activity log is at {} but DATA_DIR is {}, so new alerts go to {}. Move it there to keep its \
             history, or set ALERTS_FILE to its path.",
            old.display(),
            data_dir.display(),
            paths.alerts_file.display()
        )),
        Err(e) => Some(format!("{e:#}")),
    })
}

/// `legacy_dir/name` when it exists and `file` doesn't. That they are one file
/// (DATA_DIR naming ./data, or a link to it) is then impossible: `file` would exist.
fn left_behind(file: &Path, legacy_dir: &Path, name: &str) -> Result<Option<PathBuf>> {
    let exists = |p: &Path| p.try_exists().wrap_err_with(|| format!("Could not check whether {} exists", p.display()));
    if exists(file)? {
        return Ok(None);
    }
    let old = legacy_dir.join(name);
    Ok(exists(&old)?.then_some(old))
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
    /// `.env.example` is the user-facing list of settings: every variable the code
    /// reads must be documented there, so the two cannot drift apart.
    #[test]
    fn every_variable_the_config_reads_is_documented_in_env_example() {
        let example = include_str!("../.env.example");
        let source = include_str!("config.rs");
        let mut names = Vec::new();
        for part in source.split("env::var(\"").skip(1) {
            if let Some(name) = part.split('"').next() {
                names.push(name.to_string());
            }
        }
        assert!(names.len() >= 6, "expected to find the config variables, found {names:?}");
        for name in names {
            assert!(example.contains(&format!("# {name}=")), "{name} is read by Config but missing from .env.example");
        }
    }

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

    fn vars(data_dir: Option<&str>, markets_file: Option<&str>, alerts_file: Option<&str>) -> PathVars {
        PathVars {
            data_dir: data_dir.map(PathBuf::from),
            markets_file: markets_file.map(PathBuf::from),
            alerts_file: alerts_file.map(PathBuf::from),
        }
    }

    /// With only DATA_DIR set, the legs and the activity log go there too, not
    /// to ./data next to it (the hardened systemd unit can only write DATA_DIR).
    #[test]
    fn data_dir_holds_the_markets_and_alerts_files_by_default() {
        let paths = data_paths(&vars(Some("/var/lib/polyfarmer"), None, None));
        let dir = PathBuf::from("/var/lib/polyfarmer");
        assert_eq!(
            paths,
            DataPaths { markets_file: dir.join("markets.json"), alerts_file: dir.join("alerts.json"), data_dir: dir }
        );
    }

    #[test]
    fn without_data_dir_the_paths_are_unchanged() {
        assert_eq!(
            data_paths(&vars(None, None, None)),
            DataPaths {
                data_dir: PathBuf::from("data"),
                markets_file: PathBuf::from("data/markets.json"),
                alerts_file: PathBuf::from("data/alerts.json"),
            }
        );
    }

    #[test]
    fn explicit_markets_and_alerts_files_win() {
        let paths = data_paths(&vars(Some("/var/lib/polyfarmer"), Some("/srv/legs.json"), Some("/srv/log.json")));
        assert_eq!(paths.markets_file, PathBuf::from("/srv/legs.json"));
        assert_eq!(paths.alerts_file, PathBuf::from("/srv/log.json"));
        let paths = data_paths(&vars(None, Some("/srv/legs.json"), None));
        assert_eq!(paths.markets_file, PathBuf::from("/srv/legs.json"));
        assert_eq!(paths.alerts_file, PathBuf::from("data/alerts.json"));
    }

    /// A fresh folder standing in for the working directory: `data` in it is
    /// the old default folder, `new` the DATA_DIR.
    fn workdir() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("pf-config-{}", uuid::Uuid::new_v4()));
        let (old, new) = (root.join("data"), root.join("new"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        (root, old, new)
    }

    fn left_behind_with(data_dir: &Path, legacy_dir: &Path) -> Result<Option<String>> {
        let v = vars(Some(data_dir.to_str().unwrap()), None, None);
        files_left_behind(&v, &data_paths(&v), legacy_dir)
    }

    /// Legs still in the old place stop the start with what to do, and the
    /// file is left where it is.
    #[test]
    fn markets_left_in_the_old_folder_stop_the_start() {
        let (root, old, new) = workdir();
        std::fs::write(old.join("markets.json"), "[]").unwrap();
        let err = left_behind_with(&new, &old).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "Your markets file is at {} but DATA_DIR is {}. Move it to {}, or set MARKETS_FILE to its path.",
                old.join("markets.json").display(),
                new.display(),
                new.join("markets.json").display()
            )
        );
        assert!(old.join("markets.json").exists() && !new.join("markets.json").exists(), "nothing moved");

        // MARKETS_FILE names it: starts. Moved: starts.
        let named = vars(Some(new.to_str().unwrap()), Some(old.join("markets.json").to_str().unwrap()), None);
        assert_eq!(files_left_behind(&named, &data_paths(&named), &old).unwrap(), None);
        std::fs::rename(old.join("markets.json"), new.join("markets.json")).unwrap();
        assert_eq!(left_behind_with(&new, &old).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Both files in both places: the DATA_DIR ones are used, nothing to say.
    #[test]
    fn files_already_in_data_dir_are_used() {
        let (root, old, new) = workdir();
        for dir in [&old, &new] {
            std::fs::write(dir.join("markets.json"), "[]").unwrap();
            std::fs::write(dir.join("alerts.json"), "").unwrap();
        }
        assert_eq!(left_behind_with(&new, &old).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The activity log is only history: left behind, it is a warning.
    #[test]
    fn alerts_left_in_the_old_folder_only_warn() {
        let (root, old, new) = workdir();
        std::fs::write(old.join("alerts.json"), "").unwrap();
        let warning = left_behind_with(&new, &old).unwrap().unwrap();
        assert!(warning.starts_with(&format!("Your activity log is at {}", old.join("alerts.json").display())));
        assert!(warning.contains(&format!("new alerts go to {}", new.join("alerts.json").display())), "{warning}");
        assert!(warning.ends_with("or set ALERTS_FILE to its path."), "{warning}");
        assert!(old.join("alerts.json").exists() && !new.join("alerts.json").exists(), "nothing moved");
        let named = vars(Some(new.to_str().unwrap()), None, Some("/srv/log.json"));
        assert_eq!(files_left_behind(&named, &data_paths(&named), &old).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// No DATA_DIR, or one that is the old folder itself: the files are where
    /// they always were.
    #[test]
    fn the_old_folder_itself_is_not_left_behind() {
        let (root, old, _) = workdir();
        std::fs::write(old.join("markets.json"), "[]").unwrap();
        std::fs::write(old.join("alerts.json"), "").unwrap();
        let unset = vars(None, None, None);
        assert_eq!(files_left_behind(&unset, &data_paths(&unset), &old).unwrap(), None);
        assert_eq!(left_behind_with(&old, &old).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A DATA_DIR that links to the old folder is the old folder too.
    #[cfg(unix)]
    #[test]
    fn a_link_to_the_old_folder_is_not_left_behind() {
        let (root, old, _) = workdir();
        std::fs::write(old.join("markets.json"), "[]").unwrap();
        std::fs::write(old.join("alerts.json"), "").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&old, &link).unwrap();
        assert_eq!(left_behind_with(&link, &old).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
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
