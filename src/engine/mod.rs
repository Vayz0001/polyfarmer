//! Trading engine: WebSocket order-book tracking, quote logic, order
//! placement/cancellation via the Polymarket V2 CLOB, and the heartbeat watchdog.
//!
//! Ported from the original `poly-lp-bot` with logic unchanged — only relocated
//! under the `engine` module. `config`, `storage`, and `types` live at the crate
//! root since they are shared with the (forthcoming) web layer.

pub mod alerts;
pub mod executor;
pub mod heartbeat;
pub mod orderbook;
pub mod quoter;
pub mod ws_manager;
