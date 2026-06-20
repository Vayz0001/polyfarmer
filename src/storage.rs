use crate::types::{Alert, MarketConfig};
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
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(configs)? + "\n";
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
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
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}
