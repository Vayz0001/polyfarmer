use crate::types::{Alert, MarketConfig, RewardHistoryFile};
use eyre::Result;
use std::fs;
use std::path::Path;

// ── markets.json ──────────────────────────────────────────────────────────────

/// Load market configs from disk.
/// Returns empty vec if the file doesn't exist yet.
/// On Windows, retries once after 100ms to handle the brief window during
/// atomic rename fallback (copy+unlink) where a partial write is possible.
pub fn load_markets(path: &Path) -> Result<Vec<MarketConfig>> {
    match load_markets_inner(path) {
        Ok(v) => Ok(v),
        Err(e) => {
            // Retry once — may have caught a mid-write on Windows
            tracing::warn!("load_markets: parse failed ({}), retrying after 100ms", e);
            std::thread::sleep(std::time::Duration::from_millis(100));
            load_markets_inner(path)
        }
    }
}

fn load_markets_inner(path: &Path) -> Result<Vec<MarketConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let configs: Vec<MarketConfig> = serde_json::from_str(&raw)?;
    let (valid, invalid): (Vec<_>, Vec<_>) = configs
        .into_iter()
        .partition(|c| c.validate().is_ok());
    for c in &invalid {
        if let Err(e) = c.validate() {
            tracing::warn!("Skipping invalid market config: {}", e);
        }
    }
    Ok(valid)
}

/// Write market configs back to disk atomically (temp + rename).
/// Called by the Rust bot when auto-removing a broken market after repeated failures.
pub fn save_markets(path: &Path, configs: &[MarketConfig]) -> Result<()> {
    let data = serde_json::to_string_pretty(configs)? + "\n";
    crate::fsutil::write_private_atomic(path, data.as_bytes())
}

// ── reward_history.json ────────────────────────────────────────────────────────
// No Polymarket endpoint provides historical/range earnings (only single-day
// queries) — this is built by polling once daily and appending here.

/// Load reward history. Returns an empty file if it doesn't exist yet (day one).
pub fn load_reward_history(path: &Path) -> Result<RewardHistoryFile> {
    if !path.exists() {
        return Ok(RewardHistoryFile::default());
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(RewardHistoryFile::default());
    }
    Ok(serde_json::from_str(&raw)?)
}

/// Write reward history back to disk atomically (temp + rename).
pub fn save_reward_history(path: &Path, history: &RewardHistoryFile) -> Result<()> {
    let data = serde_json::to_string_pretty(history)? + "\n";
    crate::fsutil::write_private_atomic(path, data.as_bytes())
}

// ── alerts.json ───────────────────────────────────────────────────────────────

/// Max size of alerts.json before rotation (10 MB).
const ALERT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Append one alert to alerts.json as a newline-delimited JSON entry.
///
/// Rotates to alerts.json.1 when the file exceeds ALERT_ROTATE_BYTES,
/// keeping only the current and one previous file.
///
/// O_APPEND is atomic at the OS level for small writes, so the TS Discord bot
/// will never read a partial line.
pub fn append_alert(path: &Path, alert: &Alert) -> Result<()> {
    // Rotate if needed
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() >= ALERT_ROTATE_BYTES {
            let rotated = path.with_extension("json.1");
            // Ignore rename error — worst case we keep appending to the same file
            let _ = fs::rename(path, &rotated);
        }
    }

    let line = serde_json::to_string(alert)? + "\n";
    use std::io::Write;
    // Created owner-only: alert text can carry market names and API error detail.
    let mut file = crate::fsutil::open_private_append(path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Read up to `limit` most-recent alerts from alerts.json (newline-delimited
/// JSON), returned **newest first**. Missing file → empty. Unparseable lines
/// are skipped so one bad line never breaks the feed.
pub fn read_recent_alerts(path: &Path, limit: usize) -> Vec<Alert> {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut alerts: Vec<Alert> = raw
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Alert>(l.trim()).ok())
        .take(limit)
        .collect();
    // `.rev()` already yields newest-first; keep that order.
    alerts.truncate(limit);
    alerts
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::types::{Alert, RewardHistoryFile};
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// State files hold market names, API error text and earnings — private by
    /// default, and created that way (not chmod-ed afterwards).
    #[test]
    fn state_files_are_created_owner_only() {
        let dir = std::env::temp_dir().join(format!("pf-storage-{}", uuid::Uuid::new_v4()));
        crate::fsutil::create_private_dir(&dir).unwrap();

        let old = unsafe { libc::umask(0o000) };
        let r1 = save_markets(&dir.join("markets.json"), &[]);
        let r2 = save_reward_history(&dir.join("reward_history.json"), &RewardHistoryFile::default());
        let r3 = append_alert(&dir.join("alerts.json"), &Alert::info("hello"));
        unsafe { libc::umask(old) };
        r1.unwrap();
        r2.unwrap();
        r3.unwrap();

        for f in ["markets.json", "reward_history.json", "alerts.json"] {
            assert_eq!(mode(&dir.join(f)), 0o600, "{f}");
        }
        // Still readable by the app, and appends keep working.
        append_alert(&dir.join("alerts.json"), &Alert::warn("second")).unwrap();
        assert_eq!(read_recent_alerts(&dir.join("alerts.json"), 10).len(), 2);
        assert!(load_markets(&dir.join("markets.json")).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
