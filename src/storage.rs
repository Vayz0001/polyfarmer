use crate::types::{Alert, MarketConfig};
use eyre::Result;
use std::fs;
use std::path::Path;
use std::sync::Arc;

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
    let (valid, invalid): (Vec<_>, Vec<_>) = configs.into_iter().partition(|c| c.validate().is_ok());
    for c in &invalid {
        if let Err(e) = c.validate() {
            tracing::warn!("Skipping invalid market config: {}", e);
        }
    }
    Ok(valid)
}

/// Write market configs back to disk atomically (temp + rename).
/// Called by the engine (auto-pause, tick change, pause after rejections) and the dashboard.
pub fn save_markets(path: &Path, configs: &[MarketConfig]) -> Result<()> {
    let data = serde_json::to_string_pretty(configs)? + "\n";
    crate::fsutil::write_private_atomic(path, data.as_bytes())
}

/// Writes markets.json for the dashboard's changes: [`save_markets`] in the
/// app (`AppState::new`). A test swaps it to act at the exact moment of a write.
pub type MarketsWriter = Arc<dyn Fn(&Path, &[MarketConfig]) -> Result<()> + Send + Sync>;

// ── alerts.json ───────────────────────────────────────────────────────────────

/// Max size of alerts.json before rotation (10 MB).
const ALERT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Append one alert to alerts.json as a newline-delimited JSON entry.
///
/// Rotates to alerts.json.1 when the file exceeds ALERT_ROTATE_BYTES,
/// keeping only the current and one previous file.
///
/// O_APPEND is atomic at the OS level for small writes, so a reader never sees
/// a partial line.
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
///
/// Reads only the *end* of the file: the log can reach ~10 MB before rotation and
/// this runs per page view, so it starts with a small tail and widens it only if
/// the tail didn't hold `limit` parsable alerts.
pub fn read_recent_alerts(path: &Path, limit: usize) -> Vec<Alert> {
    use std::io::{Read, Seek, SeekFrom};

    const INITIAL_TAIL: u64 = 64 * 1024;
    if limit == 0 {
        return Vec::new();
    }
    let Ok(mut file) = fs::File::open(path) else { return Vec::new() };
    let Ok(len) = file.metadata().map(|m| m.len()) else { return Vec::new() };

    let mut want = INITIAL_TAIL;
    loop {
        let start = len.saturating_sub(want);
        let mut buf = Vec::with_capacity((len - start) as usize);
        if file.seek(SeekFrom::Start(start)).is_err() || (&mut file).take(len - start).read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        let text = String::from_utf8_lossy(&buf);
        // A tail that doesn't begin at the file start begins mid-line: drop that fragment.
        let text: &str = if start > 0 { text.split_once('\n').map(|(_, rest)| rest).unwrap_or("") } else { &text };
        let alerts: Vec<Alert> =
            text.lines().rev().filter_map(|l| serde_json::from_str::<Alert>(l.trim()).ok()).take(limit).collect();
        // Enough alerts, or we have read the whole file: done (newest first).
        if alerts.len() >= limit || start == 0 {
            return alerts;
        }
        want = want.saturating_mul(4);
    }
}

#[cfg(test)]
mod tail_tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("pf-alerts-{}.json", uuid::Uuid::new_v4()))
    }

    fn write_alerts(path: &Path, n: usize) {
        use std::io::Write;
        let mut f = fs::File::create(path).unwrap();
        for i in 0..n {
            writeln!(
                f,
                "{}",
                serde_json::to_string(&Alert::info(format!("event {i} — some typical message text here"))).unwrap()
            )
            .unwrap();
        }
    }

    #[test]
    fn returns_the_newest_alerts_first_from_a_large_file_without_reading_it_all() {
        let p = tmp();
        write_alerts(&p, 60_000); // ~6 MB, far larger than the initial 64 KiB tail
        let got = read_recent_alerts(&p, 300);
        assert_eq!(got.len(), 300);
        assert_eq!(got[0].message, "event 59999 — some typical message text here");
        assert_eq!(got[299].message, "event 59700 — some typical message text here");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn small_files_and_limits_larger_than_the_file_return_everything() {
        let p = tmp();
        write_alerts(&p, 5);
        let got = read_recent_alerts(&p, 100);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0].message, "event 4 — some typical message text here");
        assert_eq!(got[4].message, "event 0 — some typical message text here");
        assert!(read_recent_alerts(&p, 0).is_empty());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn widens_the_tail_when_the_end_of_the_file_is_mostly_garbage() {
        use std::io::Write;
        let p = tmp();
        write_alerts(&p, 10);
        // 200 KB of unparsable junk lines AFTER the real alerts: the first 64 KiB tail
        // holds no alert at all, so the reader must widen until it finds them.
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        for _ in 0..4000 {
            writeln!(f, "not json {}", "x".repeat(40)).unwrap();
        }
        let got = read_recent_alerts(&p, 3);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].message, "event 9 — some typical message text here");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn missing_and_empty_files_are_empty() {
        assert!(read_recent_alerts(&tmp(), 10).is_empty());
        let p = tmp();
        fs::write(&p, b"").unwrap();
        assert!(read_recent_alerts(&p, 10).is_empty());
        let _ = fs::remove_file(&p);
    }
}

#[cfg(all(test, unix))]
// Tests set the process umask through libc to prove files are created private.
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::types::Alert;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> std::io::Result<u32> {
        Ok(fs::metadata(p)?.permissions().mode() & 0o777)
    }

    /// State files hold market names, API error text and earnings — private by
    /// default, and created that way (not chmod-ed afterwards).
    #[test]
    fn state_files_are_created_owner_only() {
        let dir = std::env::temp_dir().join(format!("pf-storage-{}", uuid::Uuid::new_v4()));
        crate::fsutil::create_private_dir(&dir).unwrap();

        let old = unsafe { libc::umask(0o000) };
        let r1 = save_markets(&dir.join("markets.json"), &[]);
        let r2 = append_alert(&dir.join("alerts.json"), &Alert::info("hello"));
        unsafe { libc::umask(old) };
        r1.unwrap();
        r2.unwrap();

        for f in ["markets.json", "alerts.json"] {
            assert_eq!(mode(&dir.join(f)).unwrap(), 0o600, "{f}");
        }
        // Still readable by the app, and appends keep working.
        append_alert(&dir.join("alerts.json"), &Alert::warn("second")).unwrap();
        assert_eq!(read_recent_alerts(&dir.join("alerts.json"), 10).len(), 2);
        assert!(load_markets(&dir.join("markets.json")).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn leg(id: &str, token_id: &str, distance: rust_decimal::Decimal) -> MarketConfig {
        MarketConfig {
            id: id.into(),
            url: "https://polymarket.com/event/e/m".into(),
            label: "Le Pen".into(),
            condition_id: "0xabc".into(),
            token_id: token_id.into(),
            token_label: "YES".into(),
            tick_size: dec!(0.01),
            distance,
            min_depth_between: dec!(0),
            order_size: dec!(10),
            expires_at: chrono::Utc::now() + chrono::Duration::days(1),
            paused: false,
            benchmark_bid: None,
            max_volatility: None,
        }
    }

    /// A leg that fails validation is kept, paused, instead of vanishing: the
    /// next save would delete it for good and leave any resting order untracked.
    #[test]
    fn an_invalid_leg_is_loaded_paused_not_dropped() {
        let path = std::env::temp_dir().join(format!("pf-markets-{}.json", uuid::Uuid::new_v4()));
        save_markets(&path, &[leg("good", "123", dec!(0.02)), leg("bad", "456", dec!(0))]).unwrap();
        let loaded = load_markets(&path).unwrap();
        let _ = fs::remove_file(&path);
        let ids: Vec<&str> = loaded.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["good", "bad"], "both legs kept, in file order");
        assert!(!loaded[0].paused, "a valid leg is untouched");
        assert!(loaded[1].paused, "the invalid leg is paused");
        assert_eq!(loaded[1].distance, dec!(0), "its values are kept as they were, for Edit to fix");
    }
}
