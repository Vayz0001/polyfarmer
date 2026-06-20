use crate::storage::append_alert;
use crate::types::Alert;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::error;

/// Max number of alerts buffered in memory when disk writes fail.
const BUFFER_CAP: usize = 256;

/// Thin wrapper so callers don't need to handle the Result.
/// On disk write failure, alerts are buffered in memory (up to BUFFER_CAP)
/// and retried on the next send. Oldest alerts are dropped if the buffer is full.
pub struct Alerter {
    path: PathBuf,
    buffer: Mutex<VecDeque<Alert>>,
}

impl Alerter {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            buffer: Mutex::new(VecDeque::new()),
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
