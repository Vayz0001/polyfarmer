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
                (Ok(p), Ok(s)) if s > dec!(0) => { self.bids.insert(p, s); }
                (Err(_), _) => warn!("Snapshot: unparseable bid price {:?}", lvl.price),
                (_, Err(_)) => warn!("Snapshot: unparseable bid size  {:?}", lvl.size),
                _ => {}
            }
        }
        for lvl in &snap.asks {
            match (Decimal::from_str(&lvl.price), Decimal::from_str(&lvl.size)) {
                (Ok(p), Ok(s)) if s > dec!(0) => { self.asks.insert(p, s); }
                (Err(_), _) => warn!("Snapshot: unparseable ask price {:?}", lvl.price),
                (_, Err(_)) => warn!("Snapshot: unparseable ask size  {:?}", lvl.size),
                _ => {}
            }
        }

        // Best bid = last item (ascending), best ask = last item (descending)
        self.best_bid = snap.bids.last()
            .and_then(|l| Decimal::from_str(&l.price).ok());
        self.best_ask = snap.asks.last()
            .and_then(|l| Decimal::from_str(&l.price).ok());
    }

    /// Apply a single price_change entry, updating levels and top-of-book.
    /// Returns the parsed (price, size) for the caller to use if needed.
    pub fn apply_change(&mut self, entry: &WsPriceChangeEntry) -> Option<(Decimal, Decimal)> {
        let price = Decimal::from_str(&entry.price).ok()?;
        let size  = Decimal::from_str(&entry.size).ok()?;

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

    /// Total USDC depth of bids strictly between `lower` and `upper` (exclusive).
    /// Used to check: is there enough depth between our order price and best_bid?
    ///
    /// Depth (USDC) = Σ price * size  for all bids where lower < price < upper
    pub fn bid_depth_between(&self, lower: Decimal, upper: Decimal) -> Decimal {
        self.bids
            .iter()
            .filter(|(&p, _)| p > lower && p <= upper)
            .map(|(&p, &size)| p * size)
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
        // Verifies that "0.19" and "0.1900000" map to the same level.
        let mut book = TokenBook::default();
        book.bids.insert(Decimal::from_str("0.19").unwrap(), dec!(100));
        // Overwrite with different-scale representation of the same price
        book.bids.insert(Decimal::from_str("0.1900000").unwrap(), dec!(200));
        assert_eq!(book.bids.len(), 1, "should have exactly 1 level");
        assert_eq!(*book.bids.values().next().unwrap(), dec!(200));
    }

    #[test]
    fn remove_uses_decimal_not_string() {
        // Verifies that a remove with a different-scale key still works.
        let mut book = TokenBook::default();
        book.bids.insert(Decimal::from_str("0.19").unwrap(), dec!(100));

        // Simulate apply_change with price string "0.1900" (different scale)
        let price = Decimal::from_str("0.1900").unwrap();
        book.bids.remove(&price);
        assert!(book.bids.is_empty(), "level should have been removed");
    }
}
