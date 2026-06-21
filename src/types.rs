use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// ── Market config ─────────────────────────────────────────────────────────────
// Written by the TS Discord bot, read by the Rust LP engine.
// All fields are fully resolved before writing (token_id, condition_id, tick_size).

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketConfig {
    /// Stable internal ID — format: mar_<unix_secs>_<6 alpha chars>
    pub id: String,

    /// Original polymarket URL (display only)
    pub url: String,

    /// Human-readable label e.g. "US Iran — YES BUY"
    pub label: String,

    /// Resolved condition ID (needed for future user-channel WS)
    pub condition_id: String,

    /// Token ID to quote on (YES or NO)
    pub token_id: String,

    /// "YES" or "NO" (display only)
    pub token_label: String,

    /// Minimum price increment for this market (e.g. 0.01)
    /// Fetched by TS bot at resolution time via Gamma API.
    pub tick_size: Decimal,

    /// How far below best_bid to place our resting order.
    /// target_price = floor((best_bid - distance) / tick_size) * tick_size
    pub distance: Decimal,

    /// Minimum USDC depth strictly between our order price and best_bid.
    /// Order is only placed / kept alive while this condition is met.
    pub min_depth_between: Decimal,

    /// USDC size of our resting GTC order
    pub order_size: Decimal,

    /// Stop quoting after this time; cancel order and remove from active set
    pub expires_at: DateTime<Utc>,

    /// Manually paused via Discord — order cancelled, no new quotes until resumed
    #[serde(default)]
    pub paused: bool,

    /// Best bid at the time /add-market was run — used as volatility reference
    #[serde(default)]
    pub benchmark_bid: Option<Decimal>,

    /// Max allowed drift from benchmark_bid before auto-pausing (e.g. 0.03 = 3¢)
    #[serde(default)]
    pub max_volatility: Option<Decimal>,
}

impl MarketConfig {
    /// Validate that numeric fields are in sensible ranges.
    /// Called after deserializing from markets.json.
    pub fn validate(&self) -> Result<(), String> {
        if self.token_id.is_empty() {
            return Err(format!("[{}] token_id is empty", self.label));
        }
        if self.distance <= rust_decimal::Decimal::ZERO {
            return Err(format!("[{}] distance must be > 0, got {}", self.label, self.distance));
        }
        if self.order_size <= rust_decimal::Decimal::ZERO {
            return Err(format!("[{}] order_size must be > 0, got {}", self.label, self.order_size));
        }
        if self.tick_size <= rust_decimal::Decimal::ZERO {
            return Err(format!("[{}] tick_size must be > 0, got {}", self.label, self.tick_size));
        }
        if self.min_depth_between < rust_decimal::Decimal::ZERO {
            return Err(format!("[{}] min_depth_between must be >= 0, got {}", self.label, self.min_depth_between));
        }
        Ok(())
    }
}

// ── Runtime order status (in-memory only) ─────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum OrderStatus {
    /// No order on book — conditions not met or not yet evaluated
    Idle,
    /// HTTP place call is in-flight; real order_id not yet known.
    /// Quoter returns Hold — do not cancel or replace until resolved.
    Placing { price: Decimal },
    /// Resting GTC order is live on the book
    Live { order_id: String, price: Decimal },
    /// Cancel has been dispatched; waiting for confirmation before re-evaluating.
    /// `since` is used by the 30s timer to detect and recover from hung cancels.
    Cancelling { order_id: String, since: std::time::Instant },
}

impl Default for OrderStatus {
    fn default() -> Self {
        Self::Idle
    }
}

// ── Engine lifecycle phase (in-memory only) ───────────────────────────────────
// Surfaced to the dashboard so the UI can show the truth during first-run
// auto-start (no manual restart). Set by the boot task in `app.rs`.

#[derive(Debug, Clone, PartialEq, Default)]
pub enum EnginePhase {
    /// No wallet configured yet — boot task is parked waiting for one.
    #[default]
    AwaitingWallet,
    /// Wallet found; authenticating with Polymarket / cancelling stale orders.
    Starting,
    /// Engine tasks spawned and trading.
    Running,
    /// Auto-start failed (auth/network/startup-cancel). Boot task is back to
    /// waiting, so re-saving the wallet retries. Detail is logged, not shown.
    Error,
}

// ── Alert (appended to alerts.json by Rust, read+DM'd by TS Discord bot) ──────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub ts: DateTime<Utc>,
    pub level: AlertLevel,
    pub message: String,
}

impl Alert {
    pub fn info(msg: impl Into<String>) -> Self {
        Self { ts: Utc::now(), level: AlertLevel::Info, message: msg.into() }
    }
    pub fn warn(msg: impl Into<String>) -> Self {
        Self { ts: Utc::now(), level: AlertLevel::Warn, message: msg.into() }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        Self { ts: Utc::now(), level: AlertLevel::Error, message: msg.into() }
    }
}

// ── WS message types (mirror exact API shape) ─────────────────────────────────

/// One price level from the API — price and size are strings
#[derive(Debug, Clone, Deserialize)]
pub struct WsLevel {
    pub price: String,
    pub size: String,
}

/// Initial book snapshot element.
/// Arrives as a JSON array (one entry per subscribed token), no event_type wrapper.
/// Bids: ascending (worst→best). Asks: descending (worst→best).
#[derive(Debug, Clone, Deserialize)]
pub struct WsBookSnapshot {
    pub asset_id: String,
    pub bids: Vec<WsLevel>,
    pub asks: Vec<WsLevel>,
}

/// One entry in a price_change event
#[derive(Debug, Clone, Deserialize)]
pub struct WsPriceChangeEntry {
    pub asset_id: String,
    pub price: String,
    pub size: String,       // "0" = level removed
    pub side: String,       // "BUY" or "SELL"
    pub best_bid: String,   // authoritative top-of-book after this change
    pub best_ask: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WsPriceChangeEvent {
    pub price_changes: Vec<WsPriceChangeEntry>,
}

// ── Internal WS command (sent to ws_manager write task via mpsc) ──────────────

#[derive(Debug)]
pub enum WsCommand {
    Subscribe(Vec<String>),
    Unsubscribe(Vec<String>),
}
