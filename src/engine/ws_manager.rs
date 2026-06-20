use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::engine::orderbook::TokenBook;
use crate::engine::quoter::{self, DeactivateReason, QuoteAction};
use crate::storage::save_markets;
use crate::types::{
    MarketConfig, OrderStatus, WsBookSnapshot, WsPriceChangeEvent, WsCommand,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const PING_INTERVAL: Duration = Duration::from_secs(10);
const RECONNECT_BASE_MS: u64 = 100;
const RECONNECT_MAX_MS: u64 = 5_000;
pub const MAX_PLACE_FAILURES: u32 = 3;

/// Shared application state — all tasks hold an Arc clone of this.
pub struct AppState {
    /// Tokens that have received their initial book snapshot.
    /// price_change events are ignored for tokens not in this set — they'd be
    /// superseded by the snapshot and could leave the book in an inconsistent state.
    pub snapshotted: HashSet<String>,
    /// One book per token_id (shared across configs for same token)
    pub books: HashMap<String, TokenBook>,
    /// Per-config order status
    pub order_status: HashMap<String, OrderStatus>,
    /// Current market configs (refreshed by the watcher task)
    pub configs: Vec<MarketConfig>,
    /// Consecutive placement failure count per config — reset on success or reconnect
    pub place_failures: HashMap<String, u32>,
    /// Path to markets.json — needed to save after auto-removing a broken market
    pub markets_file: PathBuf,
    /// Set by heartbeat after 3 consecutive failures + order cancel.
    /// Prevents new placements until heartbeat recovers.
    pub heartbeat_paused: bool,
}

impl AppState {
    pub fn new(markets_file: PathBuf) -> Self {
        Self {
            snapshotted: HashSet::new(),
            books: HashMap::new(),
            order_status: HashMap::new(),
            configs: Vec::new(),
            place_failures: HashMap::new(),
            markets_file,
            heartbeat_paused: false,
        }
    }
}

/// Spawn the WS manager.
///
/// Owns the WebSocket connection. Reconnects automatically with backoff.
/// On disconnect: signals cancel_all, then reconnects and resubscribes.
///
/// `cmd_rx`: receives Subscribe/Unsubscribe commands from the markets watcher.
/// `stop_rx`: signals graceful shutdown.
pub fn spawn(
    state: Arc<RwLock<AppState>>,
    executor: Arc<Executor>,
    alerter: Arc<Alerter>,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
    mut stop_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut backoff_ms = RECONNECT_BASE_MS;

        loop {
            // Check for shutdown
            if *stop_rx.borrow() {
                info!("WS manager stopping");
                break;
            }

            // Collect currently subscribed token_ids from state
            let token_ids: Vec<String> = {
                let s = state.read().await;
                s.configs.iter().map(|c| c.token_id.clone()).collect::<HashSet<_>>()
                    .into_iter().collect()
            };

            // Don't connect until we have at least one market — the server
            // immediately drops connections that send no subscription.
            if token_ids.is_empty() {
                // Drain any pending Subscribe commands (they'll be replayed from
                // state on the first real connect).
                while cmd_rx.try_recv().is_ok() {}
                // Log once every ~30s so the operator knows the bot is alive but idle
                static IDLE_LOG_COUNT: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(0);
                let count = IDLE_LOG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count % 10 == 0 {
                    info!("WS manager: no markets configured, waiting... (use /add-market to add one)");
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    _ = stop_rx.changed() => { if *stop_rx.borrow() { break; } }
                }
                continue;
            }

            info!("WS connecting ({} tokens)...", token_ids.len());

            let (ws_stream, _) = match connect_async(WS_URL).await {
                Ok(s) => { backoff_ms = RECONNECT_BASE_MS; s }
                Err(e) => {
                    error!("WS connect failed: {}", e);
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
                    continue;
                }
            };

            info!("WS connected");
            alerter.info("WS connected");

            let (mut write, mut read) = ws_stream.split();

            // Subscribe all active token_ids immediately
            if !token_ids.is_empty() {
                let sub = json!({
                    "assets_ids": token_ids,
                    "type": "market",
                    "custom_feature_enabled": true
                });
                info!("WS sending subscribe: {}", sub);
                if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                    error!("WS initial subscribe failed: {}", e);
                    continue;
                }
            }

            // PING interval
            let mut ping_ticker = tokio::time::interval(PING_INTERVAL);

            // Track subscribed tokens so we can diff subscribe/unsubscribe
            let mut subscribed: HashSet<String> = token_ids.into_iter().collect();

            let disconnect_reason = loop {
                tokio::select! {
                    // ── Incoming WS message ─────────────────────────────────
                    msg = read.next() => {
                        const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024; // 64 MB
                    match msg {
                            Some(Ok(Message::Text(text))) => {
                                if text.len() > MAX_MESSAGE_BYTES {
                                    warn!("WS message too large ({} bytes) — discarding", text.len());
                                    continue;
                                }
                                if text.trim() == "PONG" { continue; }
                                handle_message(
                                    &text,
                                    &state,
                                    &executor,
                                    &alerter,
                                ).await;
                            }
                            Some(Ok(Message::Binary(data))) => {
                                if data.len() > MAX_MESSAGE_BYTES {
                                    warn!("WS binary message too large ({} bytes) — discarding", data.len());
                                    continue;
                                }
                                debug!("WS recv binary ({} bytes)", data.len());
                                if let Ok(text) = std::str::from_utf8(&data) {
                                    handle_message(text, &state, &executor, &alerter).await;
                                } else {
                                    warn!("WS binary message is not valid UTF-8");
                                }
                            }
                            Some(Ok(Message::Ping(data))) => {
                                debug!("WS recv Ping — sending Pong");
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Some(Ok(Message::Close(_))) => break "server closed",
                            Some(Err(e)) => break Box::leak(format!("recv error: {e}").into_boxed_str()),
                            None => break "stream ended",
                            _ => {}
                        }
                    }

                    // ── PING ────────────────────────────────────────────────
                    _ = ping_ticker.tick() => {
                        if write.send(Message::Text("PING".into())).await.is_err() {
                            break "ping send failed";
                        }
                    }

                    // ── Subscribe/Unsubscribe commands ──────────────────────
                    Some(cmd) = cmd_rx.recv() => {
                        match cmd {
                            WsCommand::Subscribe(tokens) => {
                                let new: Vec<String> = tokens.into_iter()
                                    .filter(|t| !subscribed.contains(t))
                                    .collect();
                                if !new.is_empty() {
                                    let msg = json!({
                                        "assets_ids": new,
                                        "operation": "subscribe",
                                        "custom_feature_enabled": true
                                    });
                                    if write.send(Message::Text(msg.to_string().into())).await.is_err() {
                                        break "subscribe send failed";
                                    }
                                    new.iter().for_each(|t| { subscribed.insert(t.clone()); });
                                    info!("WS subscribed {} new tokens", new.len());
                                }
                            }
                            WsCommand::Unsubscribe(tokens) => {
                                let to_remove: Vec<String> = tokens.into_iter()
                                    .filter(|t| subscribed.contains(t))
                                    .collect();
                                if !to_remove.is_empty() {
                                    let msg = json!({ "assets_ids": to_remove, "operation": "unsubscribe" });
                                    if write.send(Message::Text(msg.to_string().into())).await.is_err() {
                                        break "unsubscribe send failed";
                                    }
                                    to_remove.iter().for_each(|t| { subscribed.remove(t); });
                                    // Free book memory and snapshot state for removed tokens
                                    let mut s = state.write().await;
                                    for token in &to_remove {
                                        s.books.remove(token);
                                        s.snapshotted.remove(token);
                                    }
                                }
                            }
                        }
                    }

                    // ── Graceful shutdown ───────────────────────────────────
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() { break "shutdown"; }
                    }
                }
            };

            if disconnect_reason == "shutdown" {
                break;
            }

            // Collect live and in-flight-cancel order IDs before any state mutation.
            // Include Cancelling because a cancel may not have completed before the disconnect —
            // re-cancelling is idempotent and prevents orphaned orders.
            let live_order_ids: Vec<String> = {
                let s = state.read().await;
                s.order_status.values().filter_map(|status| {
                    match status {
                        OrderStatus::Live { order_id, .. } |
                        OrderStatus::Cancelling { order_id, .. } => Some(order_id.clone()),
                        _ => None,
                    }
                }).collect()
            };

            warn!("WS disconnected: {} — cancelling {} bot orders", disconnect_reason, live_order_ids.len());
            alerter.warn(format!("WS disconnected ({}) — cancelling {} open orders", disconnect_reason, live_order_ids.len()));

            // Retry up to 3 times — only reset statuses on confirmed cancellation.
            // If we reset to Idle without confirming, the bot re-places orders that are still
            // open on the CLOB, creating duplicate positions.
            let mut cancel_confirmed = false;
            for attempt in 1u32..=3 {
                match executor.cancel_orders(&live_order_ids).await {
                    Ok(_) => { cancel_confirmed = true; break; }
                    Err(e) => {
                        error!("cancel_orders attempt {}/3 failed: {}", attempt, e);
                        if attempt < 3 {
                            tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                        }
                    }
                }
            }

            if cancel_confirmed {
                // Reset statuses to Idle so the quoter can re-quote on reconnect.
                // Only clear place_failures for configs that had a live order —
                // those failures were placement successes (order got onto the book),
                // so the underlying config is valid. Configs that were Idle/Placing
                // keep their failure count: a reconnect doesn't fix a broken config.
                let mut s = state.write().await;
                let mut live_config_ids: HashSet<String> = HashSet::new();
                for (id, status) in s.order_status.iter_mut() {
                    if matches!(status, OrderStatus::Live { .. }) {
                        live_config_ids.insert(id.clone());
                    }
                    *status = OrderStatus::Idle;
                }
                for id in &live_config_ids {
                    s.place_failures.remove(id);
                }
                // Books will be rebuilt from snapshots on reconnect
                s.snapshotted.clear();
                s.books.clear();
            } else {
                // Could not confirm cancellation — keep statuses dirty so the quoter
                // won't place new orders (it only places from Idle). Alert for manual check.
                alerter.error(
                    "cancel_orders failed after 3 attempts on WS disconnect — \
                     manual check required, bot will not re-quote until restart"
                );
            }

            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
        }
    });
}

/// Handle one incoming WS text message.
async fn handle_message(
    text: &str,
    state: &Arc<RwLock<AppState>>,
    executor: &Arc<Executor>,
    alerter: &Arc<Alerter>,
) {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => { warn!("WS JSON parse error: {}", e); return; }
    };

    // Initial book snapshot = JSON array
    if let Some(arr) = value.as_array() {
        let snapshots: Vec<WsBookSnapshot> = arr.iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();

        let mut s = state.write().await;
        for snap in snapshots {
            let asset_id = snap.asset_id.clone();
            let book = s.books.entry(asset_id.clone()).or_default();
            book.apply_snapshot(&snap);
            let (best_bid, best_ask, n_bids, n_asks) =
                (book.best_bid, book.best_ask, book.bids.len(), book.asks.len());
            s.snapshotted.insert(asset_id.clone());
            info!("Book snapshot: {}... | best_bid={:?} best_ask={:?} bids={} asks={}",
                asset_id.get(..8).unwrap_or(&asset_id),
                best_bid, best_ask, n_bids, n_asks);
        }
        return;
    }

    let event_type = value.get("event_type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        "price_change" => {
            let event: WsPriceChangeEvent = match serde_json::from_str(text) {
                Ok(e) => e,
                Err(e) => { warn!("price_change parse error: {}", e); return; }
            };

            // Collect which tokens were affected
            let affected_tokens: HashSet<String> = event.price_changes.iter()
                .map(|c| c.asset_id.clone())
                .collect();

            // Apply changes to books — skip tokens that haven't received their snapshot yet.
            // Price changes arriving before the snapshot would be wiped when the snapshot
            // arrives (apply_snapshot calls clear()), leaving the book in an inconsistent state.
            {
                let mut s = state.write().await;
                for change in &event.price_changes {
                    if !s.snapshotted.contains(&change.asset_id) {
                        continue; // snapshot hasn't arrived yet — will get authoritative state soon
                    }
                    let book = s.books.entry(change.asset_id.clone()).or_default();
                    book.apply_change(change);
                }
            }

            // Evaluate and act for each affected config
            // Read state, collect actions, then execute outside the lock
            let actions: Vec<(String, QuoteAction, String, rust_decimal::Decimal, rust_decimal::Decimal)> = {
                let s = state.read().await;
                s.configs.iter()
                    .filter(|c| affected_tokens.contains(&c.token_id))
                    .filter_map(|c| {
                        // Book may not be populated yet if snapshot hasn't arrived.
                        // Skip rather than panic — next event will re-evaluate.
                        let book = s.books.get(&c.token_id)?;
                        let status = s.order_status.get(&c.id).cloned().unwrap_or_default();
                        let action = quoter::evaluate(c, book, &status);
                        // Log depth from current order price (if Live) or target (otherwise),
                        // matching what the quoter actually checks.
                        let depth_lower = if let OrderStatus::Live { price, .. } = &status {
                            *price
                        } else {
                            book.best_bid.unwrap_or_default() - c.distance
                        };
                        let depth = book.bid_depth_between(
                            depth_lower,
                            book.best_bid.unwrap_or_default(),
                        );
                        if matches!(action, QuoteAction::Hold) {
                            debug!("[{}] best_bid={:?} depth={} status={:?} → Hold",
                                &c.label, book.best_bid, depth, status);
                        } else {
                            info!("[{}] best_bid={:?} depth={} status={:?} → {:?}",
                                &c.label, book.best_bid, depth, status, action);
                        }
                        Some((c.id.clone(), action, c.token_id.clone(), c.order_size, c.tick_size))
                    })
                    .collect()
            };

            execute_actions(actions, state, executor, alerter).await;
        }

        "last_trade_price" => {
            // Trade executed — log only, no action needed for LP bot
            let asset_id = value.get("asset_id").and_then(|v| v.as_str()).unwrap_or("?");
            let price    = value.get("price").and_then(|v| v.as_str()).unwrap_or("?");
            let size     = value.get("size").and_then(|v| v.as_str()).unwrap_or("?");
            info!("Trade: asset={}... price={} size={}", asset_id.get(..8).unwrap_or(asset_id), price, size);
        }

        "market_resolved" => {
            info!("Market resolved: {}", value.get("market").and_then(|v| v.as_str()).unwrap_or("?"));
        }

        _ => {}
    }
}

/// Execute a batch of quote actions. Sets order status optimistically before
/// the network call to prevent double-placement from concurrent events.
async fn execute_actions(
    actions: Vec<(String, QuoteAction, String, rust_decimal::Decimal, rust_decimal::Decimal)>,
    state: &Arc<RwLock<AppState>>,
    executor: &Arc<Executor>,
    alerter: &Arc<Alerter>,
) {
    for (config_id, action, token_id, order_size, _tick_size) in actions {
        match action {
            QuoteAction::Hold => {}

            QuoteAction::Place { price } => {
                // Optimistic status — prevents double-placement while HTTP call is in-flight
                {
                    let mut s = state.write().await;
                    if s.heartbeat_paused {
                        continue; // CLOB unreachable — don't attempt placement
                    }
                    let status = s.order_status.entry(config_id.clone()).or_default();
                    if *status != OrderStatus::Idle {
                        continue; // race guard
                    }
                    *status = OrderStatus::Placing { price };
                }

                let exec = Arc::clone(executor);
                let state2 = Arc::clone(state);
                let alerter2 = Arc::clone(alerter);
                tokio::spawn(async move {
                    let shares = match Executor::shares_from_usd(order_size, price) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("Place: invalid price {}: {}", price, e);
                            let mut s = state2.write().await;
                            if matches!(s.order_status.get(&config_id), Some(OrderStatus::Placing { .. })) {
                                s.order_status.insert(config_id, OrderStatus::Idle);
                            }
                            return;
                        }
                    };
                    match exec.place_buy_order(&token_id, price, shares).await {
                        Ok(order_id) => {
                            let mut s = state2.write().await;
                            // Commit only if still Placing at this exact price.
                            // A reconnect or deactivate may have reset state while the call was in-flight.
                            let still_placing = matches!(
                                s.order_status.get(&config_id),
                                Some(OrderStatus::Placing { price: p }) if *p == price
                            );
                            if still_placing {
                                s.order_status.insert(config_id.clone(), OrderStatus::Live {
                                    order_id: order_id.clone(),
                                    price,
                                });
                                s.place_failures.remove(&config_id); // reset on success
                                let (label, token_label) = s.configs.iter()
                                    .find(|c| c.id == config_id)
                                    .map(|c| (c.label.clone(), c.token_label.clone()))
                                    .unwrap_or_else(|| ("unknown".to_string(), "?".to_string()));
                                drop(s);
                                alerter2.info(format!(
                                    "Order Placed: `{}`\nBUY {} {} Shares @ {}c for {}$",
                                    label,
                                    shares,
                                    token_label,
                                    price * rust_decimal_macros::dec!(100),
                                    order_size
                                ));
                            } else {
                                drop(s);
                                // State was externally changed while placing — orphan cancel.
                                warn!("State changed during placement — cancelling orphan {}", order_id);
                                let _ = exec.cancel_order(&order_id).await;
                            }
                        }
                        Err(e) => {
                            let err_str = e.to_string();

                            // "Not enough balance" = collateral locked by a matching/settling order.
                            // Not a config failure — just wait for next price_change to re-evaluate.
                            if err_str.contains("not enough balance") {
                                warn!("Place skipped: insufficient balance (order settling?) — will retry on next tick");
                                let mut s = state2.write().await;
                                if matches!(s.order_status.get(&config_id), Some(OrderStatus::Placing { .. })) {
                                    s.order_status.insert(config_id, OrderStatus::Idle);
                                }
                                return;
                            }

                            error!("Place order failed: {}", e);
                            alerter2.error(format!("Order placement failed: {}", e));

                            // Collect auto-remove data under the lock, but don't mutate
                            // configs until after save succeeds — prevents state/disk divergence.
                            let auto_remove = {
                                let mut s = state2.write().await;
                                if !matches!(s.order_status.get(&config_id), Some(OrderStatus::Placing { .. })) {
                                    None // state changed while in-flight
                                } else {
                                    s.order_status.insert(config_id.clone(), OrderStatus::Idle);
                                    let failures = s.place_failures.entry(config_id.clone()).or_insert(0);
                                    *failures += 1;
                                    if *failures < MAX_PLACE_FAILURES {
                                        None
                                    } else {
                                        let label = s.configs.iter()
                                            .find(|c| c.id == config_id)
                                            .map(|c| c.label.clone())
                                            .unwrap_or_else(|| config_id.to_string());
                                        let remaining: Vec<_> = s.configs.iter()
                                            .filter(|c| c.id != config_id)
                                            .cloned()
                                            .collect();
                                        let markets_file = s.markets_file.clone();
                                        Some((label, remaining, markets_file))
                                    }
                                }
                            };

                            if let Some((label, remaining, markets_file)) = auto_remove {
                                match save_markets(&markets_file, &remaining) {
                                    Ok(_) => {
                                        let mut s = state2.write().await;
                                        s.configs.retain(|c| c.id != config_id);
                                        s.order_status.remove(&config_id);
                                        s.place_failures.remove(&config_id);
                                        drop(s);
                                        alerter2.error(format!(
                                            "Market '{}' auto-removed after {} consecutive placement failures",
                                            label, MAX_PLACE_FAILURES
                                        ));
                                    }
                                    Err(e) => {
                                        error!("Failed to save markets after auto-remove of '{}': {}", label, e);
                                        alerter2.error(format!(
                                            "Market '{}' hit {} failures but save failed — will retry on next failure: {}",
                                            label, MAX_PLACE_FAILURES, e
                                        ));
                                        // State unchanged — failures stays at MAX, next failure retries save.
                                    }
                                }
                            }
                        }
                    }
                });
            }

            QuoteAction::Cancel { order_id, reason } => {
                let (label, token_label) = {
                    let mut s = state.write().await;
                    s.order_status.insert(config_id.clone(), OrderStatus::Cancelling {
                        order_id: order_id.clone(),
                        since: std::time::Instant::now(),
                    });
                    s.configs.iter()
                        .find(|c| c.id == config_id)
                        .map(|c| (c.label.clone(), c.token_label.clone()))
                        .unwrap_or_else(|| ("unknown".to_string(), "?".to_string()))
                };

                let exec = Arc::clone(executor);
                let state2 = Arc::clone(state);
                let alerter2 = Arc::clone(alerter);
                let reason_str = format!("{:?}", reason);
                tokio::spawn(async move {
                    let confirmed = match exec.cancel_order_verified(&order_id).await {
                        Ok(true) => {
                            alerter2.info(format!(
                                "Order Cancelled: `{}`\n{}$ of {} Shares\nReason: {}\n{}",
                                label,
                                order_size,
                                token_label,
                                reason_str,
                                chrono::Local::now().format("%I:%M %p")
                            ));
                            true
                        }
                        Ok(false) => {
                            // Verified: order still active on CLOB — leave Cancelling.
                            // 30s timeout in evaluate_all_markets will reset to Idle.
                            warn!("Order {} still on CLOB after verified cancel — leaving Cancelling for timeout recovery", order_id);
                            alerter2.warn(format!(
                                "Order {} cancel unconfirmed (still live) — will retry via 30s timer",
                                order_id
                            ));
                            false
                        }
                        Err(e) => {
                            // HTTP/network error — leave in Cancelling, let the 30s timeout recover
                            error!("Cancel order {} failed: {}", order_id, e);
                            false
                        }
                    };
                    if confirmed {
                        let mut s = state2.write().await;
                        // Guard: only reset to Idle if still Cancelling this exact order.
                        if matches!(s.order_status.get(&config_id),
                            Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == &order_id)
                        {
                            s.order_status.insert(config_id, OrderStatus::Idle);
                        }
                    }
                });
            }

            QuoteAction::Replace { order_id, new_price } => {
                {
                    let mut s = state.write().await;
                    if s.heartbeat_paused {
                        continue; // CLOB unreachable — don't cancel+replace
                    }
                    s.order_status.insert(config_id.clone(), OrderStatus::Cancelling {
                        order_id: order_id.clone(),
                        since: std::time::Instant::now(),
                    });
                }

                let exec = Arc::clone(executor);
                let state2 = Arc::clone(state);
                let alerter2 = Arc::clone(alerter);
                tokio::spawn(async move {
                    // Cancel first, with CLOB verification on ambiguous responses.
                    // Ok(true)  = confirmed off CLOB — safe to place.
                    // Ok(false) = verified still active — leave Cancelling; 30s timeout recovers.
                    // Err(_)    = network failure — leave Cancelling; 30s timeout recovers.
                    match exec.cancel_order_verified(&order_id).await {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!("Replace: cancel of {} verified still live — leaving Cancelling for timeout recovery", order_id);
                            return; // leave state as Cancelling; 30s timeout will reset to Idle
                        }
                        Err(e) => {
                            error!("Replace: cancel {} failed — aborting replace: {}", order_id, e);
                            // Leave state as Cancelling; 30s timeout will reset to Idle.
                            return;
                        }
                    }

                    // Cancel confirmed. Re-read the book now — the market may have moved
                    // during the cancel network call, so new_price (captured at decision time)
                    // could be stale. Compute a fresh target from the current book state.
                    let (fresh_price, fresh_size) = {
                        let s = state2.read().await;
                        match (s.books.get(&token_id), s.configs.iter().find(|c| c.id == config_id)) {
                            (Some(book), Some(cfg)) => {
                                match book.best_bid {
                                    Some(bb) => {
                                        use crate::engine::orderbook::TokenBook;
                                        use rust_decimal_macros::dec;
                                        let raw = bb - cfg.distance;
                                        if raw <= dec!(0) {
                                            // Degenerate — reset to Idle, quoter will re-evaluate
                                            drop(s);
                                            let mut s = state2.write().await;
                                            if matches!(s.order_status.get(&config_id),
                                                Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == &order_id)
                                            {
                                                s.order_status.insert(config_id, OrderStatus::Idle);
                                            }
                                            return;
                                        }
                                        (TokenBook::snap_to_tick(raw, cfg.tick_size), cfg.order_size)
                                    }
                                    // No book data yet — fall back to the original decision price
                                    None => (new_price, order_size),
                                }
                            }
                            // Config was removed while cancel was in-flight; state guard below will catch it
                            _ => (new_price, order_size),
                        }
                    };

                    let shares = match Executor::shares_from_usd(fresh_size, fresh_price) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("Replace: invalid price {}: {}", fresh_price, e);
                            let mut s = state2.write().await;
                            if matches!(s.order_status.get(&config_id),
                                Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == &order_id)
                            {
                                s.order_status.insert(config_id, OrderStatus::Idle);
                            }
                            return;
                        }
                    };
                    match exec.place_buy_order(&token_id, fresh_price, shares).await {
                        Ok(new_order_id) => {
                            let mut s = state2.write().await;
                            // Guard: only commit if still Cancelling this exact order.
                            // A timeout-reset may have changed state while cancel/place were in-flight.
                            let still_cancelling = matches!(
                                s.order_status.get(&config_id),
                                Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == &order_id
                            );
                            if still_cancelling {
                                let (label, token_label) = s.configs.iter()
                                    .find(|c| c.id == config_id)
                                    .map(|c| (c.label.clone(), c.token_label.clone()))
                                    .unwrap_or_else(|| ("unknown".to_string(), "?".to_string()));
                                s.order_status.insert(config_id, OrderStatus::Live {
                                    order_id: new_order_id,
                                    price: fresh_price,
                                });
                                drop(s);
                                alerter2.info(format!(
                                    "Order Replaced: `{}`\nBUY {} {} Shares @ {}c for {}$",
                                    label,
                                    shares,
                                    token_label,
                                    fresh_price * rust_decimal_macros::dec!(100),
                                    order_size
                                ));
                            } else {
                                drop(s);
                                warn!("Replace: state changed during cancel+place — cancelling orphan {}", new_order_id);
                                let _ = exec.cancel_order(&new_order_id).await;
                            }
                        }
                        Err(e) => {
                            error!("Replace: place failed: {}", e);
                            alerter2.error(format!("Replace order failed: {}", e));
                            let mut s = state2.write().await;
                            if matches!(s.order_status.get(&config_id),
                                Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == &order_id)
                            {
                                s.order_status.insert(config_id, OrderStatus::Idle);
                            }
                        }
                    }
                });
            }

            QuoteAction::Deactivate { order_id, reason } => {
                let label = {
                    let s = state.read().await;
                    s.configs.iter().find(|c| c.id == config_id)
                        .map(|c| c.label.clone()).unwrap_or_else(|| config_id.clone())
                };

                if matches!(reason, DeactivateReason::Volatility) {
                    // Auto-pause so the bot doesn't re-place while the market is swinging
                    let markets_file = {
                        let mut s = state.write().await;
                        if let Some(cfg) = s.configs.iter_mut().find(|c| c.id == config_id) {
                            cfg.paused = true;
                        }
                        s.markets_file.clone()
                    };
                    let configs = state.read().await.configs.clone();
                    if let Err(e) = save_markets(&markets_file, &configs) {
                        error!("Failed to save markets after volatility pause of '{}': {}", label, e);
                    }
                    alerter.warn(format!(
                        "Market Auto-Paused: `{}`\nVolatility threshold exceeded — use /resume-market when stable",
                        label
                    ));
                } else {
                    let (title, msg) = match reason {
                        DeactivateReason::Paused     => ("Market Paused", "Order cancelled — use /resume-market to re-enable"),
                        DeactivateReason::Expired    => ("Market Expired", "Config expired and has been deactivated"),
                        DeactivateReason::Volatility => unreachable!(),
                    };
                    alerter.info(format!("{}: `{}`\n{}", title, label, msg));
                }

                match order_id {
                    Some(oid) => {
                        // Set Cancelling synchronously before spawning — subsequent price_change
                        // events will see Cancelling and return Hold, preventing duplicate alerts.
                        {
                            let mut s = state.write().await;
                            s.order_status.insert(config_id.clone(), OrderStatus::Cancelling {
                                order_id: oid.clone(),
                                since: std::time::Instant::now(),
                            });
                        }
                        let exec = Arc::clone(executor);
                        let state2 = Arc::clone(state);
                        tokio::spawn(async move {
                            let _ = exec.cancel_order_verified(&oid).await;
                            let mut s = state2.write().await;
                            if matches!(s.order_status.get(&config_id),
                                Some(OrderStatus::Cancelling { order_id: existing, .. }) if existing == &oid)
                            {
                                s.order_status.insert(config_id, OrderStatus::Idle);
                            }
                        });
                    }
                    None => {
                        // No real order exists yet (Idle or Placing).
                        // Reset to Idle immediately — if Placing, the in-flight task will
                        // see the state change and cancel the orphan order itself.
                        let mut s = state.write().await;
                        s.order_status.insert(config_id, OrderStatus::Idle);
                    }
                }
            }
        }
    }
}
