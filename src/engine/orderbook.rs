use crate::types::{WsBookSnapshot, WsPriceChangeEntry};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::str::FromStr;
use tracing::warn;

/// Local orderbook state for a single token.
///
/// Levels are stored as price (Decimal) → size (Decimal).
/// Using Decimal keys avoids duplicate entries when the API sends the same
/// price in different string forms (e.g. "0.19" vs "0.1900000").
/// rust_decimal normalizes before hashing so numerically equal values collide correctly.
#[derive(Debug, Default)]
pub struct TokenBook {
    /// price → size. Only contains levels with size > 0.
    pub bids: HashMap<Decimal, Decimal>,
    pub asks: HashMap<Decimal, Decimal>,

    /// Authoritative top-of-book, updated from price_change.best_bid/best_ask.
    /// None until we receive the first event.
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
}

impl TokenBook {
    /// Populate from initial WS snapshot array element.
    /// Bids arrive ascending (worst→best), asks descending (worst→best).
    /// Best bid = last item in bids, best ask = last item in asks.
    pub fn apply_snapshot(&mut self, snap: &WsBookSnapshot) {
        self.bids.clear();
        self.asks.clear();

        for lvl in &snap.bids {
            match (Decimal::from_str(&lvl.price), Decimal::from_str(&lvl.size)) {
                (Ok(p), Ok(s)) if s > dec!(0) => {
                    self.bids.insert(p, s);
                }
                (Err(_), _) => warn!("Snapshot: unparseable bid price {:?}", lvl.price),
                (_, Err(_)) => warn!("Snapshot: unparseable bid size  {:?}", lvl.size),
                _ => {}
            }
        }
        for lvl in &snap.asks {
            match (Decimal::from_str(&lvl.price), Decimal::from_str(&lvl.size)) {
                (Ok(p), Ok(s)) if s > dec!(0) => {
                    self.asks.insert(p, s);
                }
                (Err(_), _) => warn!("Snapshot: unparseable ask price {:?}", lvl.price),
                (_, Err(_)) => warn!("Snapshot: unparseable ask size  {:?}", lvl.size),
                _ => {}
            }
        }

        // Top of book from the levels themselves, not their order in the message.
        self.best_bid = self.bids.keys().max().copied();
        self.best_ask = self.asks.keys().min().copied();
    }

    /// The book snapshots in one market-channel message. Polymarket sends them as
    /// a JSON array (on subscribe) and as single `book` objects (after trades, and
    /// whenever it resends a full book). Both carry the whole book for a token;
    /// missing the single ones left our copy stale. Shared by the engine and the
    /// dashboard's book hub.
    pub fn snapshots_in(value: &serde_json::Value) -> Vec<WsBookSnapshot> {
        let items: Vec<&serde_json::Value> = match value {
            serde_json::Value::Array(items) => items.iter().collect(),
            v if v.get("event_type").and_then(serde_json::Value::as_str) == Some("book") => vec![v],
            _ => Vec::new(),
        };
        items.into_iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect()
    }

    /// Apply a single price_change entry, updating levels and top-of-book.
    /// Returns the parsed (price, size) for the caller to use if needed.
    pub fn apply_change(&mut self, entry: &WsPriceChangeEntry) -> Option<(Decimal, Decimal)> {
        let price = Decimal::from_str(&entry.price).ok()?;
        let size = Decimal::from_str(&entry.size).ok()?;

        let map = if entry.side == "BUY" { &mut self.bids } else { &mut self.asks };

        if size == dec!(0) {
            map.remove(&price);
        } else {
            map.insert(price, size);
        }

        // Update authoritative top-of-book from the event (no scanning needed)
        if !entry.best_bid.is_empty() {
            self.best_bid = Decimal::from_str(&entry.best_bid).ok();
        }
        if !entry.best_ask.is_empty() {
            self.best_ask = Decimal::from_str(&entry.best_ask).ok();
        }

        Some((price, size))
    }

    /// Total USDC depth of bids above `lower` up to and including `upper` (the
    /// best bid): what sits ahead of an order at `lower`. The engine uses
    /// [`Self::others_depth_between`], which leaves our own orders out.
    ///
    /// Depth (USDC) = Σ price * size  for all bids where lower < price <= upper
    pub fn bid_depth_between(&self, lower: Decimal, upper: Decimal) -> Decimal {
        self.bids
            .iter()
            .filter(|(&p, _)| p > lower && p <= upper)
            .map(|(&p, &size)| p * size)
            .fold(dec!(0), |acc, x| acc + x)
    }

    /// Like [`Self::bid_depth_between`], but without our own resting orders
    /// (`own`: price and USD of each), which sit in the public book but are no
    /// protection for us. Each level is reduced by the USD we have at that
    /// price, never below zero.
    pub fn others_depth_between(&self, lower: Decimal, upper: Decimal, own: &[(Decimal, Decimal)]) -> Decimal {
        self.bids
            .iter()
            .filter(|(&p, _)| p > lower && p <= upper)
            .map(|(&p, &size)| {
                let mine: Decimal = own.iter().filter(|(op, _)| *op == p).map(|(_, usd)| *usd).sum();
                (p * size - mine).max(dec!(0))
            })
            .fold(dec!(0), |acc, x| acc + x)
    }

    /// Snap a price DOWN to the nearest valid tick.
    /// For BUY orders this guarantees our order is never closer to best_bid
    /// than the configured distance.
    ///
    /// floor(price / tick) * tick
    pub fn snap_to_tick(price: Decimal, tick: Decimal) -> Decimal {
        if tick == dec!(0) {
            return price;
        }
        (price / tick).floor() * tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// Message shapes as Polymarket sends them (seen live, Nov 2026): an array of
    /// books on subscribe, and single `book` objects after trades. Both count;
    /// other events don't.
    #[test]
    fn book_snapshots_come_as_arrays_and_as_single_objects() {
        let book = |id: &str| {
            serde_json::json!({"market":"0xm","asset_id":id,"event_type":"book","timestamp":"1",
                "bids":[{"price":"0.30","size":"10"},{"price":"0.32","size":"5"}],
                "asks":[{"price":"0.36","size":"7"},{"price":"0.34","size":"3"}]})
        };
        let arr = serde_json::Value::Array(vec![book("a"), book("b")]);
        assert_eq!(TokenBook::snapshots_in(&arr).len(), 2);
        let single = TokenBook::snapshots_in(&book("c"));
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].asset_id, "c");
        for other in [
            serde_json::json!({"event_type":"price_change","price_changes":[]}),
            serde_json::json!({"event_type":"last_trade_price","asset_id":"a","price":"0.3"}),
            serde_json::json!({"event_type":"best_bid_ask","asset_id":"a"}),
            serde_json::json!({"event_type":"new_market","id":"1"}),
        ] {
            assert!(TokenBook::snapshots_in(&other).is_empty(), "{other}");
        }

        // Top of book comes from the levels, whatever order they arrive in.
        let mut tb = TokenBook::default();
        tb.apply_snapshot(&single[0]);
        assert_eq!((tb.best_bid, tb.best_ask), (Some(dec!(0.32)), Some(dec!(0.34))));
        // A later single snapshot replaces the book entirely.
        let mut after = book("c");
        after["bids"] = serde_json::json!([{"price":"0.31","size":"9"}]);
        tb.apply_snapshot(&TokenBook::snapshots_in(&after)[0]);
        assert_eq!(tb.best_bid, Some(dec!(0.31)));
        assert_eq!(tb.bids.len(), 1, "old levels gone");
    }

    #[test]
    fn others_depth_leaves_out_our_own_orders() {
        let mut book = TokenBook::default();
        book.bids.insert(dec!(0.20), dec!(500)); // $100, someone else's
        book.bids.insert(dec!(0.19), dec!(3000)); // $570: $500 of it is ours
                                                  // Plain depth counts every bid:
        assert_eq!(book.bid_depth_between(dec!(0.18), dec!(0.20)), dec!(670));
        // Without ours: only the other people's $170.
        assert_eq!(book.others_depth_between(dec!(0.18), dec!(0.20), &[(dec!(0.19), dec!(500))]), dec!(170));
        // A level that is all ours counts nothing (never negative), whatever rounding left.
        let own = [(dec!(0.19), dec!(600)), (dec!(0.20), dec!(100))];
        assert_eq!(book.others_depth_between(dec!(0.18), dec!(0.20), &own), dec!(0));
        // Orders of ours outside the range don't change it; none at all = the plain depth.
        assert_eq!(book.others_depth_between(dec!(0.19), dec!(0.20), &[(dec!(0.19), dec!(500))]), dec!(100));
        assert_eq!(book.others_depth_between(dec!(0.18), dec!(0.20), &[]), dec!(670));
    }

    #[test]
    fn snap_to_tick_exact() {
        assert_eq!(TokenBook::snap_to_tick(dec!(0.19), dec!(0.01)), dec!(0.19));
    }

    #[test]
    fn snap_to_tick_rounds_down() {
        // 0.195 with tick 0.01 → 0.19 (further from best_bid, never closer)
        assert_eq!(TokenBook::snap_to_tick(dec!(0.195), dec!(0.01)), dec!(0.19));
    }

    #[test]
    fn snap_to_tick_rounds_down_2() {
        assert_eq!(TokenBook::snap_to_tick(dec!(0.199), dec!(0.01)), dec!(0.19));
    }

    #[test]
    fn bid_depth_between_basic() {
        let mut book = TokenBook::default();
        book.bids.insert(dec!(0.18), dec!(100));
        book.bids.insert(dec!(0.19), dec!(200));
        book.bids.insert(dec!(0.20), dec!(50));
        book.bids.insert(dec!(0.21), dec!(500)); // best_bid (included)
        book.bids.insert(dec!(0.17), dec!(100)); // below lower (excluded)

        // depth between 0.17 (excl) and 0.21 (incl) → 0.18*100 + 0.19*200 + 0.20*50 + 0.21*500
        let depth = book.bid_depth_between(dec!(0.17), dec!(0.21));
        let expected = dec!(0.18) * dec!(100) + dec!(0.19) * dec!(200) + dec!(0.20) * dec!(50) + dec!(0.21) * dec!(500);
        assert_eq!(depth, expected);
    }

    #[test]
    fn decimal_key_deduplicates_equivalent_price_strings() {
        let mut book = TokenBook::default();
        book.bids.insert(Decimal::from_str("0.19").unwrap(), dec!(100));
        // Overwrite with different-scale representation of the same price
        book.bids.insert(Decimal::from_str("0.1900000").unwrap(), dec!(200));
        assert_eq!(book.bids.len(), 1, "should have exactly 1 level");
        assert_eq!(*book.bids.values().next().unwrap(), dec!(200));
    }

    #[test]
    fn remove_uses_decimal_not_string() {
        let mut book = TokenBook::default();
        book.bids.insert(Decimal::from_str("0.19").unwrap(), dec!(100));

        // Simulate apply_change with price string "0.1900" (different scale)
        let price = Decimal::from_str("0.1900").unwrap();
        book.bids.remove(&price);
        assert!(book.bids.is_empty(), "level should have been removed");
    }
}
