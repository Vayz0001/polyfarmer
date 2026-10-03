//! Web dashboard: axum server serving an embedded HTML UI on localhost.
//!
//! Single-binary: templates (Askama, compiled in) + static assets (rust-embed)
//! are baked into the executable. Auth is session-based (tower-sessions) with
//! argon2 passwords; secrets live encrypted in [`crate::creds`].

mod activity;
mod assets;
mod auth;
pub mod book_hub;
mod dashboard;
pub mod events;
pub mod limiter;
mod markets;
mod positions;
mod rewards;
mod router;
pub mod session_store;
mod setup;
mod shell;
mod state;

pub use markets::prewarm_browse;
pub use router::router;
pub use state::{EngineHandle, WebState};
