//! Daily reward-history snapshot. Polymarket has no historical/range earnings
//! endpoint — only single-day queries — so history is built by polling once a
//! day and appending to a local atomic-written log.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{Timelike, Utc};
use tokio::sync::watch;

use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::storage::{load_reward_history, save_reward_history};
use crate::types::RewardSnapshot;

/// Re-check cadence. Cheap (one network call + a small local file), and
/// idempotent, so hourly is plenty — see `maybe_snapshot`'s file-based guard.
const POLL_INTERVAL: Duration = Duration::from_secs(3600);
/// Don't attempt before this UTC hour — Polymarket's daily payout settles
/// "after midnight UTC"; querying too early risks an incomplete total.
const MIN_HOUR_UTC: u32 = 1;

/// Spawn the daily reward-history poller. Runs one immediate check on boot
/// (so a restart at any time of day catches up quickly, rather than waiting
/// up to an hour) then re-checks hourly until shutdown.
pub fn spawn(
    executor: Arc<Executor>,
    alerter: Arc<Alerter>,
    path: PathBuf,
    mut stop_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        maybe_snapshot(&executor, &alerter, &path).await;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {
                    maybe_snapshot(&executor, &alerter, &path).await;
                }
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() { break; }
                }
            }
        }
    });
}

/// Idempotency guard lives in the file itself (no separate marker) — restart-
/// safe by construction, since the file on disk is the only state that
/// survives a restart and every tick re-derives correctness from it.
async fn maybe_snapshot(executor: &Arc<Executor>, alerter: &Arc<Alerter>, path: &Path) {
    let now = Utc::now();
    if now.hour() < MIN_HOUR_UTC {
        return;
    }

    let yesterday = (now - chrono::Duration::days(1)).date_naive();

    let mut history = match load_reward_history(path) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("reward history: failed to read {}: {}", path.display(), e);
            return;
        }
    };
    if history.snapshots.iter().any(|s| s.date == yesterday) {
        return; // already captured — no-op
    }

    match executor.total_earnings_for_user_for_day(yesterday).await {
        Ok(entries) => {
            let total: rust_decimal::Decimal = entries.iter().map(|e| e.earnings).sum();
            history.snapshots.push(RewardSnapshot {
                date: yesterday,
                total_earnings: total,
                captured_at: now,
            });
            if let Err(e) = save_reward_history(path, &history) {
                tracing::error!("reward history: failed to save {}: {}", path.display(), e);
                alerter.error(format!(
                    "Reward snapshot for {yesterday} computed but saving it failed: {e}"
                ));
            }
        }
        Err(e) => {
            // Transient failures are expected to self-heal on the next hourly
            // tick — no alert spam, matching the pattern used for the boot
            // retry loop in app.rs.
            tracing::warn!("reward history: fetch failed for {}: {} (retrying next tick)", yesterday, e);
        }
    }
}
