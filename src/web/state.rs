//! Shared web state: the credential store + a tiny in-memory login rate-limiter.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::creds::CredentialStore;

/// Lock the dashboard after this many consecutive failed logins.
pub const MAX_LOGIN_FAILS: u32 = 5;
/// Lockout duration once the fail threshold is hit.
pub const LOCKOUT_SECS: u64 = 30;

#[derive(Clone)]
pub struct WebState {
    pub store: Arc<CredentialStore>,
    pub login_guard: Arc<Mutex<LoginGuard>>,
}

#[derive(Default)]
pub struct LoginGuard {
    pub fails: u32,
    pub locked_until: Option<Instant>,
}

impl WebState {
    pub fn new(store: Arc<CredentialStore>) -> Self {
        Self {
            store,
            login_guard: Arc::new(Mutex::new(LoginGuard::default())),
        }
    }
}
