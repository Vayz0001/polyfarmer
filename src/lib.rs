//! polyfarmer — self-hosted Polymarket LP-rewards bot.
//!
//! Single binary: the trading [`engine`] and the [`web`] dashboard run together
//! in one process. Exposed as a library so the router and engine are testable.

pub mod app;
pub mod config;
pub mod creds;
pub mod engine;
pub mod storage;
pub mod types;
pub mod web;
