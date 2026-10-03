//! Trading engine: WebSocket order-book tracking, quote logic, order
//! placement/cancellation via the Polymarket V2 CLOB, and the heartbeat watchdog.
//!
//! `config`, `storage`, and `types` live at the crate root since they are shared
//! with the web layer.

pub mod alerts;
pub mod executor;
pub mod heartbeat;
pub mod orderbook;
pub mod quoter;
pub mod ws_manager;
