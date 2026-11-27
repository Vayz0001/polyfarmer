use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::engine::orderbook::TokenBook;
use crate::engine::pairing;
use crate::engine::placer;
use crate::engine::quoter::QuoteAction;
use crate::engine::runner;
use crate::types::{MarketConfig, OrderStatus, WsCommand, WsPriceChangeEvent};
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
/// Longest a connection attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Nothing heard (no book update, no reply to our PING every 10 s) for this
/// long: the connection is treated as dead — a half-open socket would
/// otherwise leave the bot quoting on a frozen book.
const SILENCE_LIMIT: Duration = Duration::from_secs(30);
/// A connection must last this long before the reconnect delay starts over;
/// a server that accepts and drops at once gets a growing delay, not a loop.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// While the feed keeps flapping, connect/disconnect alerts at most this often.
const FEED_ALERT_EVERY: Duration = Duration::from_secs(300);

/// The connection has been silent for longer than [`SILENCE_LIMIT`].
fn silent_too_long(last_heard: std::time::Instant, now: std::time::Instant) -> bool {
    now.saturating_duration_since(last_heard) > SILENCE_LIMIT
}

/// The reconnect delay after a connection that lasted `lasted`: back to the
/// base only after a stable one, otherwise doubled (capped).
fn next_backoff_ms(current: u64, lasted: Duration) -> u64 {
    if lasted >= STABLE_AFTER {
        RECONNECT_BASE_MS
    } else {
        (current * 2).min(RECONNECT_MAX_MS)
    }
}

/// Whether a feed alert may go out now (at most one per [`FEED_ALERT_EVERY`]).
fn feed_alert_due(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t) >= FEED_ALERT_EVERY)
}
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
    /// Earliest next placement attempt per config after a failure (backoff).
    /// Cleared on success and when the leg is removed.
    pub place_retry_at: HashMap<String, tokio::time::Instant>,
    /// Which placement attempt owns a leg's `Placing` status. A task that
    /// finishes late (after a reconnect or pause reset the leg and a newer
    /// attempt claimed it) must not touch the newer attempt's state.
    pub place_attempt: HashMap<String, u64>,
    /// Source of attempt ids.
    pub next_attempt: u64,
    /// Unconfirmed cancel retries per leg: (order id, failed tries). The count
    /// restarts whenever the leg is cancelling a different order.
    pub cancel_failures: HashMap<String, (String, u32)>,
    /// Path to markets.json — saved by engine-side changes (auto-pause, tick change, pause after rejections)
    pub markets_file: PathBuf,
    /// How the dashboard writes markets.json (`storage::save_markets`).
    pub markets_writer: crate::storage::MarketsWriter,
    /// Set by heartbeat after 3 consecutive failures + order cancel.
    /// Prevents new placements until heartbeat recovers.
    pub heartbeat_paused: bool,
    /// Engine lifecycle phase — driven by the boot task, read by the dashboard
    /// to show first-run auto-start progress without a manual restart.
    pub engine_phase: crate::types::EnginePhase,
    /// Whether the engine's market WebSocket is currently connected. Display
    /// only (dashboard status strip) — no trading decision reads it.
    pub ws_connected: bool,
    /// When the last WS message arrived (display only).
    pub last_ws_msg: Option<std::time::Instant>,
    /// When the last successful heartbeat completed (display only).
    pub last_heartbeat_ok: Option<std::time::Instant>,
    /// Two-sided rule (`engine::pairing`): since when a live leg has been
    /// without a live order on the other outcome.
    pub pair_alone_since: HashMap<String, crate::engine::pairing::AloneClock>,
    /// Two-sided rule: how many times a leg was pulled for being alone in a
    /// row (sets its back-off; reset once both sides are live together).
    pub pair_pulls: HashMap<String, u32>,
    /// Two-sided rule: a pulled leg's back-off (kept apart from placement
    /// failures' `place_retry_at`, and only applied while the rule is in force).
    pub pair_retry_at: HashMap<String, tokio::time::Instant>,
    /// Two-sided rule: legs held off the book by it, and why (display).
    pub pair_wait: HashMap<String, crate::engine::pairing::PairWait>,
    /// "Not enough balance": when a leg first hit it (one short retry is allowed,
    /// as collateral can be briefly locked while a fill settles).
    pub low_balance_since: HashMap<String, std::time::Instant>,
    /// Legs paused because Polymarket said there wasn't enough balance (display;
    /// cleared on resume or pause).
    pub low_balance_paused: HashSet<String>,
    /// Legs whose order was just taken down for a re-peg: their next placement
    /// is announced as "Order replaced" (`engine::runner`).
    pub repegging: HashSet<String>,
    /// The market feed dropped and hasn't reconnected: books are cleared and
    /// nothing is placed (`placer::begin_place`) until it's back.
    pub feed_down: bool,
    /// Placement failures in a row that were outages (network, 5xx, 429), not
    /// rejections: they only back off (`placer::outage`).
    pub place_outage: HashMap<String, u32>,
    /// Shutting down: nothing new is placed (`placer::begin_place`).
    pub stopping: bool,
    /// Tokens whose open orders must be checked before any leg on them places
    /// again: a placement there failed in a way that leaves it unclear whether
    /// the order reached Polymarket (`placer::reconcile`).
    pub reconcile_tokens: HashSet<String>,
    /// Tokens being checked right now (one check per token at a time).
    pub reconciling: HashSet<String>,
    /// Legs with a re-peg in progress (one at a time per leg, `engine::runner`).
    pub repegging_now: HashSet<String>,
    /// A re-peg's tick read failed: no new re-peg for this leg until then.
    pub repeg_retry_at: HashMap<String, tokio::time::Instant>,
}

impl AppState {
    pub fn new(markets_file: PathBuf) -> Self {
        Self {
            snapshotted: HashSet::new(),
            books: HashMap::new(),
            order_status: HashMap::new(),
            configs: Vec::new(),
            place_failures: HashMap::new(),
            place_retry_at: HashMap::new(),
            place_attempt: HashMap::new(),
            next_attempt: 0,
            cancel_failures: HashMap::new(),
            markets_file,
            markets_writer: Arc::new(crate::storage::save_markets),
            heartbeat_paused: false,
            engine_phase: crate::types::EnginePhase::AwaitingWallet,
            ws_connected: false,
            last_ws_msg: None,
            last_heartbeat_ok: None,
            pair_alone_since: HashMap::new(),
            pair_pulls: HashMap::new(),
            pair_retry_at: HashMap::new(),
            pair_wait: HashMap::new(),
            low_balance_since: HashMap::new(),
            low_balance_paused: HashSet::new(),
            repegging: HashSet::new(),
            feed_down: false,
            place_outage: HashMap::new(),
            stopping: false,
            reconcile_tokens: HashSet::new(),
            reconciling: HashSet::new(),
            repegging_now: HashSet::new(),
            repeg_retry_at: HashMap::new(),
        }
    }

    /// The tokens the legs farm, each once: what the feed subscribes to and
    /// what the startup, timer and shutdown checks list open orders on. An
    /// empty token id (an invalid leg, loaded paused) is left out: it names no
    /// market, and the order library would read it as token 0.
    pub fn farmed_tokens(&self) -> Vec<String> {
        self.configs
            .iter()
            .filter(|c| !c.token_id.is_empty())
            .map(|c| c.token_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
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
    nudge: Arc<tokio::sync::Notify>,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
    mut stop_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut backoff_ms = RECONNECT_BASE_MS;
        // Throttles the connect/disconnect alerts while the feed flaps.
        let mut last_feed_alert: Option<std::time::Instant> = None;
        let mut disconnect_alerted = false;

        loop {
            // Check for shutdown
            if *stop_rx.borrow() {
                info!("WS manager stopping");
                break;
            }

            // Collect currently subscribed token_ids from state
            let token_ids: Vec<String> = state.read().await.farmed_tokens();

            // Don't connect until we have at least one market — the server
            // immediately drops connections that send no subscription.
            if token_ids.is_empty() {
                // Drain any pending Subscribe commands (they'll be replayed from
                // state on the first real connect).
                while cmd_rx.try_recv().is_ok() {}
                // Log once every ~30s so the operator knows the bot is alive but idle
                static IDLE_LOG_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let count = IDLE_LOG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count.is_multiple_of(10) {
                    info!("WS manager: no markets configured, waiting... (add one from the dashboard)");
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    _ = stop_rx.changed() => { if *stop_rx.borrow() { break; } }
                }
                continue;
            }

            info!("WS connecting ({} tokens)...", token_ids.len());

            let connected = match tokio::time::timeout(CONNECT_TIMEOUT, connect_async(WS_URL)).await {
                Ok(Ok((s, _))) => Ok(s),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("no answer within {}s", CONNECT_TIMEOUT.as_secs())),
            };
            let ws_stream = match connected {
                Ok(s) => s,
                Err(e) => {
                    error!("WS connect failed: {}", e);
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
                    continue;
                }
            };
            let connected_at = std::time::Instant::now();
            let (mut write, mut read) = ws_stream.split();

            // Subscribe all active token_ids immediately. If that fails the
            // connection is useless: wait and reconnect (still marked down).
            let sub = json!({
                "assets_ids": token_ids,
                "type": "market",
                "custom_feature_enabled": true
            });
            info!("WS sending subscribe: {}", sub);
            if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                error!("WS initial subscribe failed: {}", e);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
                continue;
            }

            info!("WS connected");
            {
                let mut s = state.write().await;
                s.ws_connected = true;
                s.feed_down = false;
            }
            // Tell the user it's back only if they were told it went down.
            if disconnect_alerted {
                alerter.info("WS connected");
                disconnect_alerted = false;
            }
            let mut last_heard = std::time::Instant::now();

            // PING interval
            let mut ping_ticker = tokio::time::interval(PING_INTERVAL);

            // Track subscribed tokens so we can diff subscribe/unsubscribe
            let mut subscribed: HashSet<String> = token_ids.into_iter().collect();

            let disconnect_reason = loop {
                tokio::select! {
                    // ── Incoming WS message ─────────────────────────────────
                    msg = read.next() => {
                        const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024; // 64 MB
                        if matches!(msg, Some(Ok(_))) {
                            last_heard = std::time::Instant::now();
                        }
                    match msg {
                            Some(Ok(Message::Text(text))) => {
                                if text.len() > MAX_MESSAGE_BYTES {
                                    warn!("WS message too large ({} bytes) — discarding", text.len());
                                    continue;
                                }
                                state.write().await.last_ws_msg = Some(std::time::Instant::now());
                                if text.trim() == "PONG" { continue; }
                                handle_message(&text, &state, &executor, &alerter, &nudge).await;
                            }
                            Some(Ok(Message::Binary(data))) => {
                                if data.len() > MAX_MESSAGE_BYTES {
                                    warn!("WS binary message too large ({} bytes) — discarding", data.len());
                                    continue;
                                }
                                debug!("WS recv binary ({} bytes)", data.len());
                                if let Ok(text) = std::str::from_utf8(&data) {
                                    handle_message(text, &state, &executor, &alerter, &nudge).await;
                                } else {
                                    warn!("WS binary message is not valid UTF-8");
                                }
                            }
                            Some(Ok(Message::Ping(data))) => {
                                debug!("WS recv Ping — sending Pong");
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Some(Ok(Message::Close(_))) => break "server closed".to_string(),
                            Some(Err(e)) => break format!("recv error: {e}"),
                            None => break "stream ended".to_string(),
                            _ => {}
                        }
                    }

                    // ── PING ────────────────────────────────────────────────
                    _ = ping_ticker.tick() => {
                        if silent_too_long(last_heard, std::time::Instant::now()) {
                            break format!("no messages for {}s", SILENCE_LIMIT.as_secs());
                        }
                        if write.send(Message::Text("PING".into())).await.is_err() {
                            break "ping send failed".to_string();
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
                                        break "subscribe send failed".to_string();
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
                                        break "unsubscribe send failed".to_string();
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
                        if *stop_rx.borrow() { break "shutdown".to_string(); }
                    }
                }
            };

            state.write().await.ws_connected = false;
            if disconnect_reason == "shutdown" {
                break;
            }

            // Block placement first, then collect the orders to cancel — in one
            // step, so nothing can be placed between the two. The books are
            // stale from here on and are rebuilt from snapshots on reconnect.
            // Cancelling ones are included: a cancel may not have completed before
            // the disconnect, and re-cancelling is idempotent.
            let live_order_ids: Vec<String> = {
                let mut s = state.write().await;
                s.feed_down = true;
                s.books.clear();
                s.snapshotted.clear();
                s.order_status
                    .values()
                    .filter_map(|status| match status {
                        OrderStatus::Live { order_id, .. } | OrderStatus::Cancelling { order_id, .. } => {
                            Some(order_id.clone())
                        }
                        _ => None,
                    })
                    .collect()
            };

            warn!("WS disconnected: {} — cancelling {} bot orders", disconnect_reason, live_order_ids.len());
            let now = std::time::Instant::now();
            if feed_alert_due(last_feed_alert, now) {
                alerter.warn(format!(
                    "WS disconnected ({}) — cancelling {} open orders",
                    disconnect_reason,
                    live_order_ids.len()
                ));
                last_feed_alert = Some(now);
                disconnect_alerted = true;
            }

            // Retry up to 3 times — only orders confirmed gone are reset. Resetting one
            // that is still open would let the bot place a second order beside it.
            let mut remaining = live_order_ids.clone();
            let mut answered = false;
            for attempt in 1u32..=3 {
                match executor.cancel_orders(&remaining).await {
                    Ok(open) => {
                        answered = true;
                        remaining = open;
                        if remaining.is_empty() {
                            break;
                        }
                        warn!("cancel_orders attempt {}/3: {} order(s) still open", attempt, remaining.len());
                    }
                    Err(e) => error!("cancel_orders attempt {}/3 failed: {}", attempt, e),
                }
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                }
            }

            if answered {
                let resolved = {
                    let mut s = state.write().await;
                    let resolved = reset_after_bulk_cancel(&mut s, &live_order_ids, &remaining);
                    // Books will be rebuilt from snapshots on reconnect
                    s.snapshotted.clear();
                    s.books.clear();
                    resolved
                };
                for label in resolved {
                    alerter.info(format!("Cancel confirmed · {label}\nConfirmed by the reconnect's cancel."));
                }
                if !remaining.is_empty() {
                    alerter.error(format!(
                        "{} order(s) could not be confirmed cancelled after the disconnect. They are kept as \
                         cancelling and retried every 30 s; nothing new is placed on those legs until they are gone.",
                        remaining.len()
                    ));
                }
            } else {
                // No answer at all: keep every status as it is, so nothing is placed
                // over an order that may still rest. Live legs re-peg through a
                // verified cancel, and Cancelling ones are retried by the timer.
                alerter.error(
                    "Could not cancel orders after the disconnect (3 attempts failed) — check your open orders on \
                     Polymarket.",
                );
            }

            // Back to a short delay only after a connection that stayed up.
            backoff_ms = next_backoff_ms(backoff_ms, connected_at.elapsed());
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }
    });
}

/// After a reconnect's bulk cancel of `requested`, given the ones still open.
/// Only legs whose order was **in the request** are touched — an order that
/// appeared meanwhile (a placement that landed during the cancel) is left as it
/// is, tracked; resetting it would forget it and let the leg place a second one.
/// - order confirmed gone → Idle (and, if it was live, its placement-failure
///   count is cleared: the order did get on the book);
/// - order still open → kept `Cancelling` it, so nothing is placed beside it and
///   the 30 s timer keeps cancelling it;
/// - a placement in flight is freed (it cancels its own order when it lands).
///
/// Returns the labels of stuck cancels this confirmed that had already alerted.
pub fn reset_after_bulk_cancel(s: &mut AppState, requested: &[String], still_open: &[String]) -> Vec<String> {
    let mut had_live: HashSet<String> = HashSet::new();
    let mut freed: HashSet<String> = HashSet::new();
    for (id, status) in s.order_status.iter_mut() {
        let order = match status {
            OrderStatus::Live { order_id, .. } | OrderStatus::Cancelling { order_id, .. } => Some(order_id.clone()),
            _ => None,
        };
        if let Some(order_id) = &order {
            if !requested.contains(order_id) {
                continue; // not part of this cancel: leave it alone
            }
            if still_open.contains(order_id) {
                *status = OrderStatus::Cancelling { order_id: order_id.clone(), since: std::time::Instant::now() };
                continue;
            }
        }
        if matches!(status, OrderStatus::Live { .. }) {
            had_live.insert(id.clone());
        }
        *status = OrderStatus::Idle;
        freed.insert(id.clone());
    }
    for id in &had_live {
        s.place_failures.remove(id);
    }
    // Stuck cancels just confirmed: report the ones that had raised a "can't confirm" alert.
    let confirmed: Vec<(String, (String, u32))> =
        s.cancel_failures.iter().filter(|(id, _)| freed.contains(*id)).map(|(k, v)| (k.clone(), v.clone())).collect();
    for (id, _) in &confirmed {
        s.cancel_failures.remove(id);
    }
    confirmed
        .into_iter()
        .filter(|(_, (_, tries))| *tries >= placer::CANCEL_ALERT_AFTER)
        .map(|(id, _)| s.configs.iter().find(|c| c.id == id).map(|c| c.label.clone()).unwrap_or(id))
        .collect()
}

/// Handle one incoming WS text message.
async fn handle_message(
    text: &str,
    state: &Arc<RwLock<AppState>>,
    executor: &Arc<Executor>,
    alerter: &Arc<Alerter>,
    nudge: &Arc<tokio::sync::Notify>,
) {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!("WS JSON parse error: {}", e);
            return;
        }
    };

    // Full book snapshots: an array on subscribe, single `book` objects later
    // (after trades). Both replace the token's book.
    let snapshots = TokenBook::snapshots_in(&value);
    if !snapshots.is_empty() {
        let mut s = state.write().await;
        for snap in snapshots {
            let asset_id = snap.asset_id.clone();
            let book = s.books.entry(asset_id.clone()).or_default();
            book.apply_snapshot(&snap);
            let (best_bid, best_ask, n_bids, n_asks) = (book.best_bid, book.best_ask, book.bids.len(), book.asks.len());
            s.snapshotted.insert(asset_id.clone());
            info!(
                "Book snapshot: {}... | best_bid={:?} best_ask={:?} bids={} asks={}",
                asset_id.get(..8).unwrap_or(&asset_id),
                best_bid,
                best_ask,
                n_bids,
                n_asks
            );
        }
        return;
    }

    let event_type = value.get("event_type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        "price_change" => {
            let event: WsPriceChangeEvent = match serde_json::from_str(text) {
                Ok(e) => e,
                Err(e) => {
                    warn!("price_change parse error: {}", e);
                    return;
                }
            };

            // Collect which tokens were affected
            let affected_tokens: HashSet<String> = event.price_changes.iter().map(|c| c.asset_id.clone()).collect();

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

            // Evaluate and act for each affected config — and for the other
            // outcome of the same market, so a two-sided pair moves together
            // (engine::pairing). Decide under the lock, execute outside it.
            let actions: Vec<pairing::Decision> = {
                let mut s = state.write().await;
                let markets: HashSet<String> = s
                    .configs
                    .iter()
                    .filter(|c| affected_tokens.contains(&c.token_id) && !c.condition_id.is_empty())
                    .map(|c| c.condition_id.to_ascii_lowercase())
                    .collect();
                let actions = pairing::decide(
                    &mut s,
                    |c| {
                        affected_tokens.contains(&c.token_id)
                            || (!c.condition_id.is_empty() && markets.contains(&c.condition_id.to_ascii_lowercase()))
                    },
                    tokio::time::Instant::now(),
                );
                for pairing::Decision { config_id: id, action, token_id, .. } in &actions {
                    let Some(c) = s.configs.iter().find(|c| &c.id == id) else { continue };
                    let book = s.books.get(token_id);
                    let status = s.order_status.get(id).cloned().unwrap_or_default();
                    // Depth from the live order's price (or the target), as the quoter checks it.
                    let best_bid = book.and_then(|b| b.best_bid).unwrap_or_default();
                    let lower =
                        if let OrderStatus::Live { price, .. } = &status { *price } else { best_bid - c.distance };
                    let own = crate::engine::pairing::own_orders(&s, token_id);
                    let depth = book.map(|b| b.others_depth_between(lower, best_bid, &own)).unwrap_or_default();
                    if matches!(action, QuoteAction::Hold) {
                        debug!("[{}] best_bid={:?} depth={} status={:?} → Hold", &c.label, best_bid, depth, status);
                    } else {
                        info!(
                            "[{}] best_bid={:?} depth={} status={:?} → {:?}",
                            &c.label, best_bid, depth, status, action
                        );
                    }
                }
                actions
            };

            runner::execute(actions, executor, state, alerter, nudge).await;
        }

        "last_trade_price" => {
            // Trade executed — log only, no action needed for LP bot
            let asset_id = value.get("asset_id").and_then(|v| v.as_str()).unwrap_or("?");
            let price = value.get("price").and_then(|v| v.as_str()).unwrap_or("?");
            let size = value.get("size").and_then(|v| v.as_str()).unwrap_or("?");
            info!("Trade: asset={}... price={} size={}", asset_id.get(..8).unwrap_or(asset_id), price, size);
        }

        "tick_size_change" => {
            // Polymarket changed this market's tick size (e.g. 1¢ → 0.1¢).
            // Update the order library first, then the saved legs, so the next
            // quote uses the new tick and the order library accepts it.
            let Some((asset_id, new_tick)) = parse_tick_size_change(&value) else {
                warn!("tick_size_change with missing or invalid fields ignored");
                return;
            };
            let ours = state.read().await.configs.iter().any(|c| c.token_id == asset_id);
            if !ours {
                return;
            }
            if let Err(e) = executor.set_tick(&asset_id, new_tick) {
                warn!("tick_size_change ignored: {e}");
                return;
            }
            placer::apply_tick_change(state, alerter, &asset_id, new_tick).await;
        }

        "market_resolved" => {
            info!("Market resolved: {}", value.get("market").and_then(|v| v.as_str()).unwrap_or("?"));
        }

        _ => {}
    }
}

/// `(asset_id, new_tick_size)` from a `tick_size_change` market event. The
/// tick must be a positive decimal below 1; the order library additionally
/// only accepts Polymarket's known tick sizes.
fn parse_tick_size_change(value: &serde_json::Value) -> Option<(String, rust_decimal::Decimal)> {
    use std::str::FromStr;
    let asset = value.get("asset_id")?.as_str()?;
    if asset.is_empty() || asset.len() > 100 || !asset.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let tick = rust_decimal::Decimal::from_str(value.get("new_tick_size")?.as_str()?).ok()?;
    (tick > rust_decimal::Decimal::ZERO && tick < rust_decimal::Decimal::ONE).then(|| (asset.to_string(), tick))
}

#[cfg(test)]
mod feed_health_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_silent_connection_is_dropped_after_the_limit() {
        let t0 = Instant::now();
        assert!(!silent_too_long(t0, t0 + Duration::from_secs(29)));
        assert!(!silent_too_long(t0, t0 + SILENCE_LIMIT));
        assert!(silent_too_long(t0, t0 + SILENCE_LIMIT + Duration::from_millis(1)));
        // PINGs go out every 10 s, so a live feed always answers well inside it.
        assert!(PING_INTERVAL * 2 < SILENCE_LIMIT);
    }

    #[test]
    fn the_reconnect_delay_only_resets_after_a_stable_connection() {
        // A server that accepts and drops at once: the delay keeps growing.
        let mut b = RECONNECT_BASE_MS;
        for _ in 0..10 {
            b = next_backoff_ms(b, Duration::from_millis(50));
        }
        assert_eq!(b, RECONNECT_MAX_MS);
        // A connection that stayed up a minute: back to the short delay.
        assert_eq!(next_backoff_ms(RECONNECT_MAX_MS, STABLE_AFTER), RECONNECT_BASE_MS);
    }

    #[test]
    fn feed_alerts_are_throttled_while_it_flaps() {
        let t0 = Instant::now();
        assert!(feed_alert_due(None, t0));
        assert!(!feed_alert_due(Some(t0), t0 + Duration::from_secs(60)));
        assert!(feed_alert_due(Some(t0), t0 + FEED_ALERT_EVERY));
    }
}

#[cfg(test)]
mod bulk_cancel_tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// After a reconnect's bulk cancel, only legs whose order is confirmed gone go
    /// back to Idle; a leg whose order is still open keeps cancelling it, so the
    /// bot never places a second order beside it.
    #[test]
    fn only_confirmed_cancels_free_a_leg() {
        let mut s = AppState::new(PathBuf::from("unused.json"));
        let live = |id: &str| OrderStatus::Live { order_id: id.into(), price: dec!(0.3) };
        s.order_status.insert("gone".into(), live("o1"));
        s.order_status.insert("stuck".into(), live("o2"));
        s.order_status.insert(
            "was_cancelling".into(),
            OrderStatus::Cancelling { order_id: "o3".into(), since: std::time::Instant::now() },
        );
        s.order_status.insert("placing".into(), OrderStatus::Placing { price: dec!(0.3) });
        s.place_failures.insert("gone".into(), 2);
        s.place_failures.insert("stuck".into(), 2);
        s.cancel_failures.insert("was_cancelling".into(), ("o3".into(), placer::CANCEL_ALERT_AFTER));

        let requested: Vec<String> = ["o1", "o2", "o3"].map(String::from).to_vec();
        let resolved = reset_after_bulk_cancel(&mut s, &requested, &["o2".to_string()]);

        assert_eq!(s.order_status.get("gone"), Some(&OrderStatus::Idle));
        assert!(
            matches!(s.order_status.get("stuck"), Some(OrderStatus::Cancelling { order_id, .. }) if order_id == "o2")
        );
        assert_eq!(s.order_status.get("was_cancelling"), Some(&OrderStatus::Idle), "its cancel was confirmed");
        assert_eq!(s.order_status.get("placing"), Some(&OrderStatus::Idle));
        assert!(!s.place_failures.contains_key("gone"), "its order did get on the book");
        assert_eq!(s.place_failures.get("stuck"), Some(&2), "unchanged while still open");
        assert_eq!(resolved, vec!["was_cancelling".to_string()], "the alerted stuck cancel is reported once");
        assert!(s.cancel_failures.is_empty());
    }

    /// The bug: an order placed while the bulk cancel was running wasn't in the
    /// request, but the reset set its leg Idle anyway — forgetting the order and
    /// letting the leg place a second one beside it.
    #[test]
    fn an_order_placed_during_the_cancel_stays_tracked() {
        let mut s = AppState::new(PathBuf::from("unused.json"));
        let live = |id: &str| OrderStatus::Live { order_id: id.into(), price: dec!(0.3) };
        s.order_status.insert("old".into(), live("o1"));
        s.order_status.insert("landed".into(), live("o-new")); // placed after the request was sent
        s.order_status.insert(
            "cancelling_new".into(),
            OrderStatus::Cancelling { order_id: "o-x".into(), since: std::time::Instant::now() },
        );
        reset_after_bulk_cancel(&mut s, &["o1".to_string()], &[]);
        assert_eq!(s.order_status.get("old"), Some(&OrderStatus::Idle), "its order was cancelled");
        assert_eq!(s.order_status.get("landed"), Some(&live("o-new")), "not in the request: still tracked");
        assert!(
            matches!(s.order_status.get("cancelling_new"), Some(OrderStatus::Cancelling { order_id, .. }) if order_id == "o-x"),
            "a cancel started meanwhile is left to finish"
        );
    }
}

#[cfg(test)]
mod tick_event_tests {
    use super::parse_tick_size_change;
    use rust_decimal_macros::dec;
    use serde_json::json;

    #[test]
    fn parses_the_documented_message() {
        let v = json!({"event_type":"tick_size_change","market":"0x74","asset_id":"10750588","old_tick_size":"0.01","new_tick_size":"0.001","timestamp":"1782753357257"});
        assert_eq!(parse_tick_size_change(&v), Some(("10750588".to_string(), dec!(0.001))));
    }

    #[test]
    fn rejects_bad_fields() {
        for v in [
            json!({"asset_id":"1"}),
            json!({"new_tick_size":"0.01"}),
            json!({"asset_id":"","new_tick_size":"0.01"}),
            json!({"asset_id":"12a","new_tick_size":"0.01"}),
            json!({"asset_id":"1".repeat(101),"new_tick_size":"0.01"}),
            json!({"asset_id":"1","new_tick_size":"0"}),
            json!({"asset_id":"1","new_tick_size":"-0.01"}),
            json!({"asset_id":"1","new_tick_size":"1"}),
            json!({"asset_id":"1","new_tick_size":"abc"}),
            json!({"asset_id":"1","new_tick_size":0.01}),
            json!({"asset_id":1,"new_tick_size":"0.01"}),
        ] {
            assert_eq!(parse_tick_size_change(&v), None, "{v}");
        }
    }
}

#[cfg(test)]
mod farmed_tokens_tests {
    use super::AppState;
    use crate::engine::test_support::{leg, TOKEN};
    use rust_decimal_macros::dec;

    /// A leg loaded paused because its token id is empty must never reach
    /// Polymarket as token "": not in the feed's subscription, nor in the
    /// startup, timer or shutdown order checks (all built from this list).
    #[test]
    fn an_empty_token_id_is_never_farmed() {
        let mut s = AppState::new("unused.json".into());
        let mut no_token = leg("none", dec!(0.01));
        no_token.token_id = String::new();
        no_token.paused = true;
        s.configs = vec![no_token.clone()];
        assert!(s.farmed_tokens().is_empty(), "nothing to subscribe to or cancel on");

        // Other invalid values keep their token: its stray orders are still found and cancelled.
        let mut bad_distance = leg("bad", dec!(0.01));
        bad_distance.token_id = "456".into();
        bad_distance.distance = dec!(0);
        bad_distance.paused = true;
        s.configs = vec![leg("a", dec!(0.01)), no_token, bad_distance, leg("b", dec!(0.01))];
        let mut tokens = s.farmed_tokens();
        tokens.sort();
        let mut want = vec![TOKEN.to_string(), "456".to_string()];
        want.sort();
        assert_eq!(tokens, want, "each token once, the empty one left out");
    }
}
