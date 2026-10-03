use crate::engine::orderbook::TokenBook;
use crate::types::{MarketConfig, OrderStatus};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// The action the quoter wants to take for a single market config this cycle.
#[derive(Debug, PartialEq)]
pub enum QuoteAction {
    /// Place a new GTC BUY at this price (no active order exists)
    Place { price: Decimal },
    /// Cancel existing order — reason included for alerts
    Cancel { order_id: String, reason: CancelReason },
    /// Cancel existing order then immediately re-place at new price
    Replace { order_id: String, new_price: Decimal },
    /// Do nothing — conditions still met and order price is still valid
    Hold,
    /// Market expired or paused — cancel if active, then do nothing
    Deactivate { order_id: Option<String>, reason: DeactivateReason },
}

#[derive(Debug, PartialEq)]
pub enum CancelReason {
    DepthDropped,
}

#[derive(Debug, PartialEq)]
pub enum DeactivateReason {
    Expired,
    Paused,
    Volatility,
}

/// Evaluate what action to take for a single config given the current book state.
///
/// Called:
///   - On every price_change event for the token (primary trigger)
///   - On a 30s fallback timer for all active configs
///
/// Does NOT perform any I/O — pure logic, returns an action for the caller to execute.
pub fn evaluate(config: &MarketConfig, book: &TokenBook, status: &OrderStatus) -> QuoteAction {
    // ── 1. Deactivate checks (highest priority) ────────────────────────────
    // Only emit Deactivate when there is an order to cancel or the status needs
    // transitioning (Live / Cancelling / Placing). If the market is already Idle
    // and the condition persists (paused, expired, volatile), return Hold — the
    // work is done and we don't need to fire another alert on every price tick.
    if config.paused {
        let order_id = live_order_id(status);
        if order_id.is_none() && matches!(status, OrderStatus::Idle) {
            return QuoteAction::Hold;
        }
        return QuoteAction::Deactivate { order_id, reason: DeactivateReason::Paused };
    }
    if Utc::now() >= config.expires_at {
        let order_id = live_order_id(status);
        if order_id.is_none() && matches!(status, OrderStatus::Idle) {
            return QuoteAction::Hold;
        }
        return QuoteAction::Deactivate { order_id, reason: DeactivateReason::Expired };
    }
    if let (Some(benchmark), Some(max_move)) = (config.benchmark_bid, config.max_volatility) {
        if let Some(bb) = book.best_bid {
            if (bb - benchmark).abs() >= max_move {
                let order_id = live_order_id(status);
                if order_id.is_none() && matches!(status, OrderStatus::Idle) {
                    return QuoteAction::Hold;
                }
                return QuoteAction::Deactivate { order_id, reason: DeactivateReason::Volatility };
            }
        }
    }

    // ── 2. Skip if an async operation is already in flight ─────────────────
    if matches!(status, OrderStatus::Cancelling { .. } | OrderStatus::Placing { .. }) {
        return QuoteAction::Hold;
    }

    // ── 3. Need best_bid to compute target ────────────────────────────────
    let best_bid = match book.best_bid {
        Some(b) => b,
        None => return QuoteAction::Hold, // no data yet
    };

    // ── 4. Compute target price (snapped to tick, always ≤ best_bid - distance) ──
    let raw_target = best_bid - config.distance;
    if raw_target <= dec!(0) {
        // Degenerate market — best_bid is too low to quote below it
        let order_id = live_order_id(status);
        return QuoteAction::Cancel { order_id: order_id.unwrap_or_default(), reason: CancelReason::DepthDropped };
    }
    let target = TokenBook::snap_to_tick(raw_target, config.tick_size);

    match status {
        OrderStatus::Idle => {
            // Placing from scratch — check depth at the target we're about to use.
            let depth = book.bid_depth_between(target, best_bid);
            if depth >= config.min_depth_between {
                QuoteAction::Place { price: target }
            } else {
                QuoteAction::Hold // wait for depth to build up
            }
        }

        OrderStatus::Live { order_id, price: current_price } => {
            let gap = best_bid - *current_price;

            // ── Gap check first: if best_bid moved into our order we MUST move ──
            // This takes priority over the current_depth check because the order
            // position is already invalid regardless of depth — we can't stay here.
            if gap < config.distance {
                // Check whether the new target position is safe to place at.
                let target_depth = book.bid_depth_between(target, best_bid);
                if target_depth >= config.min_depth_between {
                    return QuoteAction::Replace { order_id: order_id.clone(), new_price: target };
                } else {
                    // Can't safely re-place either — cancel and wait.
                    return QuoteAction::Cancel { order_id: order_id.clone(), reason: CancelReason::DepthDropped };
                }
            }

            // ── Is our EXISTING order still protected? ─────────────────────
            // Check depth between current order price and best_bid (not target).
            // The order at current_price is only exposed if there's nothing between
            // it and best_bid — using target here would cancel a valid order just
            // because depth is thin at a position we haven't moved to yet.
            let current_depth = book.bid_depth_between(*current_price, best_bid);
            if current_depth < config.min_depth_between {
                return QuoteAction::Cancel { order_id: order_id.clone(), reason: CancelReason::DepthDropped };
            }

            // ── Should we move to follow best_bid? ────────────────────────
            // best_bid moved away — target shifted. Only replace if the new position
            // is safe. If depth at target is thin, keep the existing protected order.
            if target != *current_price {
                let target_depth = book.bid_depth_between(target, best_bid);
                if target_depth >= config.min_depth_between {
                    return QuoteAction::Replace { order_id: order_id.clone(), new_price: target };
                }
                // New target not ready yet — existing order is still protected, hold.
                return QuoteAction::Hold;
            }

            QuoteAction::Hold
        }

        OrderStatus::Cancelling { .. } | OrderStatus::Placing { .. } => QuoteAction::Hold, // handled above
    }
}

fn live_order_id(status: &OrderStatus) -> Option<String> {
    match status {
        OrderStatus::Live { order_id, .. } => Some(order_id.clone()),
        OrderStatus::Cancelling { order_id, .. } => Some(order_id.clone()),
        OrderStatus::Placing { .. } | OrderStatus::Idle => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::orderbook::TokenBook;
    use crate::types::{MarketConfig, OrderStatus};
    use chrono::{Duration, Utc};
    use rust_decimal_macros::dec;

    fn config(distance: Decimal, min_depth: Decimal) -> MarketConfig {
        MarketConfig {
            id: "mar_test_aaaaaa".to_string(),
            url: "https://polymarket.com".to_string(),
            label: "test".to_string(),
            condition_id: "0x".to_string(),
            token_id: "abc".to_string(),
            token_label: "YES".to_string(),
            tick_size: dec!(0.01),
            distance,
            min_depth_between: min_depth,
            order_size: dec!(100),
            expires_at: Utc::now() + Duration::hours(1),
            paused: false,
            benchmark_bid: None,
            max_volatility: None,
        }
    }

    fn book_with_depth(best_bid: Decimal, depth_usdc: Decimal, distance: Decimal) -> TokenBook {
        let mut book =
            TokenBook { best_bid: Some(best_bid), best_ask: Some(best_bid + dec!(0.01)), ..Default::default() };
        // Place depth at best_bid - distance/2 (between target and best_bid)
        let mid_price = best_bid - distance / dec!(2);
        let size = depth_usdc / mid_price;
        book.bids.insert(mid_price, size);
        book
    }

    #[test]
    fn places_when_conditions_met() {
        let cfg = config(dec!(0.02), dec!(100));
        let book = book_with_depth(dec!(0.21), dec!(500), dec!(0.02));
        let action = evaluate(&cfg, &book, &OrderStatus::Idle);
        assert!(matches!(action, QuoteAction::Place { price } if price == dec!(0.19)));
    }

    #[test]
    fn holds_when_depth_insufficient() {
        let cfg = config(dec!(0.02), dec!(1000));
        let book = book_with_depth(dec!(0.21), dec!(50), dec!(0.02)); // only $50 depth
        let action = evaluate(&cfg, &book, &OrderStatus::Idle);
        assert_eq!(action, QuoteAction::Hold);
    }

    #[test]
    fn cancels_when_depth_drops_between_order_and_best_bid() {
        // best_bid=0.21, order at 0.19, gap=0.02 (ok), but depth between 0.19 and 0.21 is thin
        let cfg = config(dec!(0.02), dec!(500));
        let book = book_with_depth(dec!(0.21), dec!(50), dec!(0.02)); // only $50 depth at mid
        let status = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        let action = evaluate(&cfg, &book, &status);
        assert!(matches!(action, QuoteAction::Cancel { reason: CancelReason::DepthDropped, .. }));
    }

    #[test]
    fn replaces_when_best_bid_moves_into_order() {
        // best_bid moved to 0.20 — gap = 0.20 - 0.19 = 0.01 < distance 0.02 → must move farther out
        // book_with_depth places level at best_bid - distance/2 = 0.20 - 0.01 = 0.19
        // bid_depth_between(target=0.18, best_bid=0.20) includes the level at 0.19 → $500 depth
        let cfg = config(dec!(0.02), dec!(100));
        let book = book_with_depth(dec!(0.20), dec!(500), dec!(0.02));
        let status = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        let action = evaluate(&cfg, &book, &status);
        assert!(matches!(action, QuoteAction::Replace { new_price, .. } if new_price == dec!(0.18)));
    }

    #[test]
    fn replaces_when_best_bid_moves_away() {
        // best_bid moved from 0.21 to 0.25 — our order at 0.19 is now too far out.
        // target = 0.23, depth at (0.23, 0.25) = $500, current_depth at (0.19, 0.25) = $500.
        // Expects Replace to 0.23 (move closer to maintain spread).
        let cfg = config(dec!(0.02), dec!(100));
        // Place depth at 0.24 (between new target 0.23 and best_bid 0.25) and at 0.22 (between old 0.19 and 0.25)
        let mut book = TokenBook { best_bid: Some(dec!(0.25)), best_ask: Some(dec!(0.26)), ..Default::default() };
        book.bids.insert(dec!(0.24), dec!(600)); // $144 depth between 0.23 and 0.25
        book.bids.insert(dec!(0.22), dec!(600)); // also between 0.19 and 0.25
        let status = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        let action = evaluate(&cfg, &book, &status);
        assert!(matches!(action, QuoteAction::Replace { new_price, .. } if new_price == dec!(0.23)));
    }

    #[test]
    fn holds_when_best_bid_moves_away_but_target_depth_thin() {
        // best_bid moved to 0.25, our order at 0.19 still protected by depth at 0.22.
        // But target (0.23) has no depth yet — should Hold, keep existing order.
        let cfg = config(dec!(0.02), dec!(100));
        let mut book = TokenBook { best_bid: Some(dec!(0.25)), best_ask: Some(dec!(0.26)), ..Default::default() };
        // depth only between 0.19 and 0.23 (protects current order), NOT between 0.23 and 0.25
        book.bids.insert(dec!(0.22), dec!(600));
        let status = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        let action = evaluate(&cfg, &book, &status);
        assert_eq!(action, QuoteAction::Hold);
    }

    #[test]
    fn expired_config_deactivates() {
        let mut cfg = config(dec!(0.02), dec!(100));
        cfg.expires_at = Utc::now() - Duration::seconds(1);
        let book = book_with_depth(dec!(0.21), dec!(500), dec!(0.02));
        let status = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        let action = evaluate(&cfg, &book, &status);
        assert!(matches!(action, QuoteAction::Deactivate { reason: DeactivateReason::Expired, .. }));
    }
}
