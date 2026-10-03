//! Live order books for the dashboard's market view.
//!
//! One public connection to Polymarket's market WebSocket, owned by the web
//! layer and **separate from the trading engine's** (the engine cancels every
//! live order when its connection drops, so viewer traffic must never share
//! it). Tokens are reference-counted: subscribed while at least one browser
//! tab is watching them, unsubscribed after a short grace period once the
//! last one leaves. Books are maintained with the engine's own [`TokenBook`].
//!
//! Consumers call [`BookHub::acquire`] (keep the guard alive while watching),
//! listen on [`BookHub::updates`] for changed token ids, and read
//! [`BookHub::snapshot`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::engine::orderbook::TokenBook;
use crate::rewards::market_data::{snapshot_from_token_book, BookSnapshot};
use crate::types::{WsBookSnapshot, WsPriceChangeEvent};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const PING_INTERVAL: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_BASE_MS: u64 = 250;
const RECONNECT_MAX_MS: u64 = 10_000;
/// Keep a token subscribed this long after the last viewer leaves, so a page
/// reload or side switch doesn't churn subscribe/unsubscribe.
const UNSUBSCRIBE_GRACE: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct BookHub {
    inner: Arc<Inner>,
}

struct Inner {
    books: RwLock<HashMap<String, TokenBook>>,
    /// Tokens whose initial snapshot has arrived (price changes before it are dropped).
    snapshotted: RwLock<HashSet<String>>,
    /// Viewer reference counts per token.
    refs: Mutex<HashMap<String, usize>>,
    /// Command channel to the connection task — started lazily on first use.
    cmd_tx: OnceLock<mpsc::UnboundedSender<Cmd>>,
    /// Fan-out of token ids whose book just changed.
    updates: broadcast::Sender<String>,
}

enum Cmd {
    Subscribe(String),
    Unsubscribe(String),
}

/// Holds a token subscription open; dropping it releases the reference.
pub struct BookGuard {
    inner: Arc<Inner>,
    token: String,
}

impl Drop for BookGuard {
    fn drop(&mut self) {
        let now_zero = {
            let mut refs = self.inner.refs.lock().unwrap();
            match refs.get_mut(&self.token) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                Some(_) => {
                    refs.remove(&self.token);
                    true
                }
                None => false,
            }
        };
        if !now_zero {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let inner = Arc::clone(&self.inner);
        let token = self.token.clone();
        rt.spawn(async move {
            tokio::time::sleep(UNSUBSCRIBE_GRACE).await;
            // Re-acquired during the grace period → keep it.
            if inner.refs.lock().unwrap().contains_key(&token) {
                return;
            }
            if let Some(tx) = inner.cmd_tx.get() {
                let _ = tx.send(Cmd::Unsubscribe(token));
            }
        });
    }
}

impl Default for BookHub {
    fn default() -> Self {
        Self::new()
    }
}

impl BookHub {
    pub fn new() -> Self {
        let (updates, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                books: RwLock::new(HashMap::new()),
                snapshotted: RwLock::new(HashSet::new()),
                refs: Mutex::new(HashMap::new()),
                cmd_tx: OnceLock::new(),
                updates,
            }),
        }
    }

    fn cmd_tx(&self) -> &mpsc::UnboundedSender<Cmd> {
        self.inner.cmd_tx.get_or_init(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(run(Arc::clone(&self.inner), rx));
            tx
        })
    }

    /// Start (or join) watching `token`'s book. Keep the guard while watching.
    pub fn acquire(&self, token: &str) -> BookGuard {
        let first = {
            let mut refs = self.inner.refs.lock().unwrap();
            let n = refs.entry(token.to_string()).or_insert(0);
            *n += 1;
            *n == 1
        };
        if first {
            let _ = self.cmd_tx().send(Cmd::Subscribe(token.to_string()));
        }
        BookGuard { inner: Arc::clone(&self.inner), token: token.to_string() }
    }

    /// Receiver of token ids whose book changed.
    pub fn updates(&self) -> broadcast::Receiver<String> {
        self.inner.updates.subscribe()
    }

    /// Current live book for `token`, once its snapshot has arrived.
    pub fn snapshot(&self, token: &str, tick_size: Decimal) -> Option<BookSnapshot> {
        if !self.inner.snapshotted.read().unwrap().contains(token) {
            return None;
        }
        let books = self.inner.books.read().unwrap();
        books.get(token).map(|b| snapshot_from_token_book(b, tick_size))
    }
}

/// The connection task: idle while nothing is watched; otherwise connected,
/// subscribed to every watched token, applying snapshots + deltas.
async fn run(inner: Arc<Inner>, mut cmd_rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut backoff_ms = RECONNECT_BASE_MS;
    loop {
        let wanted: Vec<String> = inner.refs.lock().unwrap().keys().cloned().collect();
        if wanted.is_empty() {
            // Idle: block until someone subscribes (or the hub is dropped).
            match cmd_rx.recv().await {
                Some(Cmd::Subscribe(_)) => continue,
                Some(Cmd::Unsubscribe(t)) => {
                    forget(&inner, &t);
                    continue;
                }
                None => return,
            }
        }

        let ws = match tokio::time::timeout(CONNECT_TIMEOUT, connect_async(WS_URL)).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => {
                warn!("book hub: connect failed: {e}");
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
                continue;
            }
            Err(_) => {
                warn!("book hub: connect timed out");
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
                continue;
            }
        };
        backoff_ms = RECONNECT_BASE_MS;
        let (mut write, mut read) = ws.split();
        let sub = json!({ "assets_ids": wanted, "type": "market", "custom_feature_enabled": true });
        if write.send(Message::Text(sub.to_string())).await.is_err() {
            continue;
        }
        info!("book hub: connected ({} tokens)", wanted.len());
        let mut subscribed: HashSet<String> = wanted.into_iter().collect();
        let mut ping = tokio::time::interval(PING_INTERVAL);

        loop {
            tokio::select! {
                msg = read.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        if text.trim() != "PONG" {
                            handle_message(&inner, &text);
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if let Ok(text) = std::str::from_utf8(&data) {
                            handle_message(&inner, text);
                        }
                    }
                    Some(Ok(Message::Ping(d))) => { let _ = write.send(Message::Pong(d)).await; }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => { debug!("book hub: recv error: {e}"); break; }
                    _ => {}
                },
                _ = ping.tick() => {
                    if write.send(Message::Text("PING".into())).await.is_err() { break; }
                }
                cmd = cmd_rx.recv() => match cmd {
                    Some(Cmd::Subscribe(t)) => {
                        if subscribed.insert(t.clone()) {
                            let m = json!({ "assets_ids": [t], "operation": "subscribe", "custom_feature_enabled": true });
                            if write.send(Message::Text(m.to_string())).await.is_err() { break; }
                        }
                    }
                    Some(Cmd::Unsubscribe(t)) => {
                        forget(&inner, &t);
                        if subscribed.remove(&t) {
                            let m = json!({ "assets_ids": [t], "operation": "unsubscribe" });
                            if write.send(Message::Text(m.to_string())).await.is_err() { break; }
                        }
                        if subscribed.is_empty() {
                            let _ = write.send(Message::Close(None)).await;
                            break; // go idle — no reason to hold a connection
                        }
                    }
                    None => return,
                },
            }
        }
        // Books are stale until the next snapshot; viewers fall back to REST.
        inner.snapshotted.write().unwrap().clear();
        debug!("book hub: disconnected");
    }
}

fn forget(inner: &Inner, token: &str) {
    inner.books.write().unwrap().remove(token);
    inner.snapshotted.write().unwrap().remove(token);
}

fn apply_snapshot(inner: &Inner, snap: &WsBookSnapshot) {
    let mut books = inner.books.write().unwrap();
    let book = books.entry(snap.asset_id.clone()).or_default();
    book.apply_snapshot(snap);
    // Don't trust level ordering for top-of-book here — derive it.
    book.best_bid = book.bids.keys().max().copied();
    book.best_ask = book.asks.keys().min().copied();
    inner.snapshotted.write().unwrap().insert(snap.asset_id.clone());
}

fn handle_message(inner: &Inner, text: &str) {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return };
    let mut changed: HashSet<String> = HashSet::new();

    let snapshots: Vec<&Value> = match &value {
        Value::Array(items) => items.iter().collect(),
        v if v.get("event_type").and_then(Value::as_str) == Some("book") => vec![v],
        _ => Vec::new(),
    };
    for v in snapshots {
        if let Ok(snap) = serde_json::from_value::<WsBookSnapshot>(v.clone()) {
            apply_snapshot(inner, &snap);
            changed.insert(snap.asset_id);
        }
    }

    if value.get("event_type").and_then(Value::as_str) == Some("price_change") {
        if let Ok(ev) = serde_json::from_value::<WsPriceChangeEvent>(value.clone()) {
            let snapshotted = inner.snapshotted.read().unwrap().clone();
            let mut books = inner.books.write().unwrap();
            for change in &ev.price_changes {
                if !snapshotted.contains(&change.asset_id) {
                    continue;
                }
                if let Some(book) = books.get_mut(&change.asset_id) {
                    book.apply_change(change);
                    changed.insert(change.asset_id.clone());
                }
            }
        }
    }

    for t in changed {
        let _ = inner.updates.send(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn hub_with(token: &str) -> Inner {
        let (updates, _) = broadcast::channel(16);
        let inner = Inner {
            books: RwLock::new(HashMap::new()),
            snapshotted: RwLock::new(HashSet::new()),
            refs: Mutex::new(HashMap::new()),
            cmd_tx: OnceLock::new(),
            updates,
        };
        inner.refs.lock().unwrap().insert(token.to_string(), 1);
        inner
    }

    #[test]
    fn applies_book_snapshot_then_price_change() {
        let inner = hub_with("T");
        // Single-object `book` event, levels in arbitrary order.
        handle_message(
            &inner,
            r#"{"event_type":"book","asset_id":"T",
            "bids":[{"price":"0.48","size":"100"},{"price":"0.47","size":"50"}],
            "asks":[{"price":"0.52","size":"10"},{"price":"0.51","size":"20"}]}"#,
        );
        {
            let books = inner.books.read().unwrap();
            let b = books.get("T").unwrap();
            assert_eq!(b.best_bid, Some(dec!(0.48)));
            assert_eq!(b.best_ask, Some(dec!(0.51)));
        }
        handle_message(
            &inner,
            r#"{"event_type":"price_change","price_changes":[
            {"asset_id":"T","price":"0.49","size":"5","side":"BUY","best_bid":"0.49","best_ask":"0.51"}]}"#,
        );
        let books = inner.books.read().unwrap();
        let b = books.get("T").unwrap();
        assert_eq!(b.best_bid, Some(dec!(0.49)));
        assert_eq!(b.bids.get(&dec!(0.49)), Some(&dec!(5)));
    }

    #[test]
    fn ignores_price_changes_before_snapshot() {
        let inner = hub_with("T");
        handle_message(
            &inner,
            r#"{"event_type":"price_change","price_changes":[
            {"asset_id":"T","price":"0.49","size":"5","side":"BUY","best_bid":"0.49","best_ask":"0.51"}]}"#,
        );
        assert!(inner.books.read().unwrap().get("T").is_none());
    }
}
