//! Web dashboard: axum server serving an embedded HTML UI on localhost.
//!
//! Single-binary: templates (Askama, compiled in) + static assets (rust-embed)
//! are baked into the executable. Auth is session-based (tower-sessions) with
//! argon2 passwords; secrets live encrypted in [`crate::creds`].

mod assets;
mod auth;
mod dashboard;
mod markets;
mod rewards;
mod router;
mod setup;
mod state;

pub use router::router;
pub use state::{EngineHandle, WebState};
