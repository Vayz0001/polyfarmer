use crate::storage::append_alert;
use crate::types::Alert;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::error;

/// Max number of alerts buffered in memory when disk writes fail.
const BUFFER_CAP: usize = 256;

/// Drop an identical message repeated within this window. The engine has two
/// execution paths (the 30s fallback timer and the WS-event handler) that can
/// both emit the same alert for one logical action; this collapses those (and
/// any other short-window duplicates) into a single event.
const DEDUP_WINDOW: Duration = Duration::from_secs(3);

/// Thin wrapper so callers don't need to handle the Result.
/// On disk write failure, alerts are buffered in memory (up to BUFFER_CAP)
/// and retried on the next send. Oldest alerts are dropped if the buffer is full.
pub struct Alerter {
    path: PathBuf,
    buffer: Mutex<VecDeque<Alert>>,
    /// Live fan-out to the dashboard (SSE). Best-effort: if there are no
    /// subscribers the send is simply dropped.
    tx: broadcast::Sender<Alert>,
    /// Last (message, time) — used to suppress duplicate emissions.
    last: Mutex<Option<(String, Instant)>>,
}

impl Alerter {
    pub fn new(path: &Path, tx: broadcast::Sender<Alert>) -> Self {
        Self {
            path: path.to_path_buf(),
            buffer: Mutex::new(VecDeque::new()),
            tx,
            last: Mutex::new(None),
        }
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.send(Alert::info(msg));
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.send(Alert::warn(msg));
    }

    pub fn error(&self, msg: impl Into<String>) {
        self.send(Alert::error(msg));
    }

    fn send(&self, alert: Alert) {
        // Suppress an identical message repeated within DEDUP_WINDOW (the
        // dual-path engine can emit the same alert twice for one action).
        {
            let mut last = self.last.lock().unwrap();
            let now = Instant::now();
            if let Some((msg, t)) = last.as_ref() {
                if *msg == alert.message && now.duration_since(*t) < DEDUP_WINDOW {
                    return;
                }
            }
            *last = Some((alert.message.clone(), now));
        }

        // Fan out to live dashboard subscribers first (best-effort).
        let _ = self.tx.send(alert.clone());

        // Drain buffered alerts first so ordering is preserved
        let mut buf = self.buffer.lock().unwrap();
        while let Some(buffered) = buf.front() {
            match append_alert(&self.path, buffered) {
                Ok(_) => { buf.pop_front(); }
                Err(_) => break, // disk still failing — stop draining, append new alert to buffer
            }
        }

        // Write the new alert
        if let Err(e) = append_alert(&self.path, &alert) {
            error!("Failed to write alert (buffering): {}", e);
            if buf.len() >= BUFFER_CAP {
                buf.pop_front(); // drop oldest to make room
            }
            buf.push_back(alert);
        }
    }
}
