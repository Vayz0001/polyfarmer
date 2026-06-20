//! Web dashboard: axum server serving an embedded HTML UI on localhost.
//!
//! Single-binary: templates (Askama, compiled in) + static assets (rust-embed)
//! are baked into the executable — no external files needed at runtime.

mod assets;
mod router;

pub use router::router;
