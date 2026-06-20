/// WS Smoke Test
///
/// Connects to the Polymarket market WebSocket, subscribes to both YES and NO
/// tokens, and displays a live orderbook. On each price_change the full updated
/// book is reprinted with the changed level marked (updated).
///
/// Run with:
///   cargo run --bin ws-smoke
///
/// Test market: "US forces enter Iran by March 31?"
///   URL: https://polymarket.com/event/us-forces-enter-iran-by/us-forces-enter-iran-by-march-31-222-191-243-517-878-439-519

use eyre::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

const YES_TOKEN: &str =
    "42750054381142639205639663180818682570869285140532640407891991570656047928885";
const NO_TOKEN: &str =
    "81697486240392901899167649997008736380137911909662773455994395620863894931973";

// ── WS message structs (kept for documentation — all fields match the API) ───

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct Level {
    price: String,
    size: String,
}

/// Initial snapshot element (inside the array response)
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct BookEvent {
    asset_id: String,
    market: String,
    bids: Vec<Level>,
    asks: Vec<Level>,
    timestamp: String,
    hash: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct PriceChangeEntry {
    asset_id: String,
    price: String,
    size: String,     // "0" = level removed
    side: String,     // "BUY" or "SELL"
    hash: String,
    best_bid: String,
    best_ask: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct PriceChangeEvent {
    market: String,
    timestamp: String,
    price_changes: Vec<PriceChangeEntry>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct LastTradePriceEvent {
    asset_id: String,
    market: String,
    price: String,
    size: String,
    side: String,
    fee_rate_bps: String,
    timestamp: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct BestBidAskEvent {
    asset_id: String,
    market: String,
    best_bid: String,
    best_ask: String,
    spread: String,
    timestamp: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct TickSizeChangeEvent {
    asset_id: String,
    market: String,
    old_tick_size: String,
    new_tick_size: String,
    timestamp: String,
}

// ── Local book state ─────────────────────────────────────────────────────────

/// Local copy of one token's orderbook. price_str → size_f64.
/// size = 0.0 means the level was removed.
#[derive(Default)]
struct TokenBook {
    bids: HashMap<String, f64>,
    asks: HashMap<String, f64>,
}

impl TokenBook {
    fn apply_snapshot(&mut self, levels_json: &[Value], side: &str) {
        let map = if side == "bids" { &mut self.bids } else { &mut self.asks };
        map.clear();
        for lvl in levels_json {
            let price = lvl.get("price").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let size: f64 = lvl.get("size").and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok()).unwrap_or(0.0);
            if !price.is_empty() && size > 0.0 {
                map.insert(price, size);
            }
        }
    }

    fn apply_change(&mut self, price: &str, size: f64, side: &str) {
        let map = if side == "BUY" { &mut self.bids } else { &mut self.asks };
        if size == 0.0 {
            map.remove(price);
        } else {
            map.insert(price.to_string(), size);
        }
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("ws_smoke=debug,info")
        .init();

    info!("=== Polymarket WS Smoke Test ===");
    info!("Market: US forces enter Iran by March 31?");
    info!("YES token: {}...{}", &YES_TOKEN[..8], &YES_TOKEN[YES_TOKEN.len()-4..]);
    info!("NO  token: {}...{}", &NO_TOKEN[..8],  &NO_TOKEN[NO_TOKEN.len()-4..]);
    info!("");

    info!("Connecting to {}...", WS_URL);
    let (ws_stream, _) = connect_async(WS_URL).await?;
    info!("Connected.");

    let (mut write, mut read) = ws_stream.split();

    let subscribe_msg = json!({
        "assets_ids": [YES_TOKEN, NO_TOKEN],
        "type": "market",
        "custom_feature_enabled": true
    });
    write.send(Message::Text(subscribe_msg.to_string().into())).await?;
    info!("Subscription sent. Waiting for events...\n");

    // Heartbeat: PING every 10s
    let mut write_clone = write;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            if write_clone.send(Message::Text("PING".into())).await.is_err() {
                break;
            }
        }
    });

    // Local book state keyed by token_id
    let mut books: HashMap<String, TokenBook> = HashMap::new();
    let mut event_counts: HashMap<String, u32> = HashMap::new();

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => { error!("WS error: {}", e); break; }
        };

        let Message::Text(text) = msg else {
            match msg {
                Message::Ping(d) => info!("[PING from server] {} bytes", d.len()),
                Message::Pong(_) => info!("[PONG from server]"),
                Message::Close(f) => { warn!("[CLOSE] {:?}", f); break; }
                _ => {}
            }
            continue;
        };

        if text.trim() == "PONG" {
            info!("[PONG] heartbeat ok");
            continue;
        }

        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warn!("Non-JSON: {} | raw: {}", e, &text[..text.len().min(200)]);
                continue;
            }
        };

        // Initial snapshot = JSON array, one entry per subscribed token
        if let Some(arr) = value.as_array() {
            *event_counts.entry("snapshot".to_string()).or_insert(0) += 1;
            for entry in arr {
                let asset_id = entry.get("asset_id").and_then(|v| v.as_str()).unwrap_or("");
                let book = books.entry(asset_id.to_string()).or_default();

                if let Some(bids) = entry.get("bids").and_then(|v| v.as_array()) {
                    book.apply_snapshot(bids, "bids");
                }
                if let Some(asks) = entry.get("asks").and_then(|v| v.as_array()) {
                    book.apply_snapshot(asks, "asks");
                }

                let label = token_label(asset_id);
                info!("[SNAPSHOT/{label}] {} bid levels, {} ask levels",
                    book.bids.len(), book.asks.len());
                print_book(label, book, &HashSet::new(), &HashSet::new());
            }
            continue;
        }

        let event_type = value.get("event_type")
            .and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        *event_counts.entry(event_type.clone()).or_insert(0) += 1;

        match event_type.as_str() {
            "price_change" => {
                match serde_json::from_str::<PriceChangeEvent>(&text) {
                    Ok(pc) => {
                        // Apply all changes and collect which prices changed per token
                        let mut changed_bids: HashMap<String, HashSet<String>> = HashMap::new();
                        let mut changed_asks: HashMap<String, HashSet<String>> = HashMap::new();

                        for change in &pc.price_changes {
                            let size: f64 = change.size.parse().unwrap_or(0.0);
                            let book = books.entry(change.asset_id.clone()).or_default();
                            book.apply_change(&change.price, size, &change.side);

                            if change.side == "BUY" {
                                changed_bids.entry(change.asset_id.clone())
                                    .or_default().insert(change.price.clone());
                            } else {
                                changed_asks.entry(change.asset_id.clone())
                                    .or_default().insert(change.price.clone());
                            }
                        }

                        // Reprint book for each affected token
                        let affected: HashSet<String> = pc.price_changes.iter()
                            .map(|c| c.asset_id.clone()).collect();

                        for asset_id in &affected {
                            let label = token_label(asset_id);
                            let empty = HashSet::new();
                            let ub = changed_bids.get(asset_id).unwrap_or(&empty);
                            let ua = changed_asks.get(asset_id).unwrap_or(&empty);
                            if let Some(book) = books.get(asset_id) {
                                info!("[PRICE_CHANGE/{label}]");
                                print_book(label, book, ub, ua);
                            }
                        }
                    }
                    Err(e) => warn!("[PRICE_CHANGE] parse error: {}", e),
                }
            }

            "last_trade_price" => {
                match serde_json::from_str::<LastTradePriceEvent>(&text) {
                    Ok(t) => {
                        let label = token_label(&t.asset_id);
                        info!("[TRADE/{label}] side={} price={} size={} fee_bps={}",
                            t.side, t.price, t.size, t.fee_rate_bps);
                    }
                    Err(e) => warn!("[TRADE] parse error: {}", e),
                }
            }

            "best_bid_ask" => {
                match serde_json::from_str::<BestBidAskEvent>(&text) {
                    Ok(bba) => {
                        let label = token_label(&bba.asset_id);
                        let bid: f64 = bba.best_bid.parse().unwrap_or(0.0);
                        let ask: f64 = bba.best_ask.parse().unwrap_or(0.0);
                        info!("[BBA/{label}] bid={} ask={} spread={} mid={:.4}",
                            bba.best_bid, bba.best_ask, bba.spread, (bid + ask) / 2.0);
                    }
                    Err(e) => warn!("[BBA] parse error: {}", e),
                }
            }

            "tick_size_change" => {
                match serde_json::from_str::<TickSizeChangeEvent>(&text) {
                    Ok(t) => {
                        let label = token_label(&t.asset_id);
                        info!("[TICK_SIZE/{label}] {} → {}", t.old_tick_size, t.new_tick_size);
                    }
                    Err(e) => warn!("[TICK_SIZE] parse error: {}", e),
                }
            }

            "new_market"      => info!("[NEW MARKET]\n{}", pretty(&value)),
            "market_resolved" => info!("[MARKET RESOLVED]\n{}", pretty(&value)),
            other             => warn!("[UNKNOWN event_type={other}]\n{}", pretty(&value)),
        }

        let total: u32 = event_counts.values().sum();
        if total % 20 == 0 {
            info!("--- counts: {:?} ---", event_counts);
        }
    }

    info!("=== Final counts: {:?} ===", event_counts);
    Ok(())
}

// ── Display ───────────────────────────────────────────────────────────────────

/// Print the 4 levels closest to the spread on each side.
/// updated_bids / updated_asks: prices that changed in this event → marked (updated).
///
/// Display layout:
///   ASKS  worst → best  (highest price at top, best ask closest to spread)
///   ── spread ──
///   BIDS  best → worst  (best bid closest to spread, lowest price at bottom)
fn print_book(
    label: &str,
    book: &TokenBook,
    updated_bids: &HashSet<String>,
    updated_asks: &HashSet<String>,
) {
    let header  = format!("  {:>6} | {:>12} | {:>12}", "Price", "Shares", "USD Depth");
    let divider = format!("  {}-+-{}-+-{}", "------", "------------", "------------");

    // ── Asks: sort ascending, take 4 lowest (closest to spread), display reversed ──
    let mut ask_levels: Vec<(f64, &str, f64)> = book.asks.iter()
        .filter_map(|(p, &s)| p.parse::<f64>().ok().map(|pf| (pf, p.as_str(), s)))
        .collect();
    ask_levels.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap()); // ascending
    let ask_closest: Vec<_> = ask_levels.iter().take(4).collect(); // 4 best asks

    info!("  [{label}] ASKS (worst → best, 4 closest to spread):");
    info!("{header}");
    info!("{divider}");
    for (price_f, price_s, size) in ask_closest.iter().rev() { // reverse = worst on top
        let usd = price_f * size;
        let marker = if updated_asks.contains(*price_s) { " (updated)" } else { "" };
        info!("  {price_s:>6} | {size:>12.2} | ${usd:>11.2}{marker}");
    }

    info!("  ── spread ──");

    // ── Bids: sort descending, take 4 highest (closest to spread) ──
    let mut bid_levels: Vec<(f64, &str, f64)> = book.bids.iter()
        .filter_map(|(p, &s)| p.parse::<f64>().ok().map(|pf| (pf, p.as_str(), s)))
        .collect();
    bid_levels.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap()); // descending
    let bid_closest: Vec<_> = bid_levels.iter().take(4).collect(); // 4 best bids

    info!("  [{label}] BIDS (best → worst, 4 closest to spread):");
    info!("{header}");
    info!("{divider}");
    for (price_f, price_s, size) in &bid_closest {
        let usd = price_f * size;
        let marker = if updated_bids.contains(*price_s) { " (updated)" } else { "" };
        info!("  {price_s:>6} | {size:>12.2} | ${usd:>11.2}{marker}");
    }

    // Summary line
    if let (Some(best_bid), Some(best_ask)) = (bid_closest.first(), ask_closest.first()) {
        let mid    = (best_bid.0 + best_ask.0) / 2.0;
        let spread = best_ask.0 - best_bid.0;
        info!("  best_bid={} best_ask={} mid={mid:.4} spread={spread:.4}",
            best_bid.1, best_ask.1);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn token_label(asset_id: &str) -> &'static str {
    match asset_id {
        YES_TOKEN => "YES",
        NO_TOKEN  => "NO",
        _         => "UNKNOWN",
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}
