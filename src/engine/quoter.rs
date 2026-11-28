use crate::engine::orderbook::TokenBook;
use crate::types::{MarketConfig, OrderStatus};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// The action the quoter wants to take for a single market config this cycle.
#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub enum CancelReason {
    DepthDropped,
    /// Two-sided-only market and the other outcome's order isn't up
    /// (`engine::pairing`).
    OtherSideDown,
    /// Two-sided-only market and only this outcome is farmed (`engine::pairing`).
    OneSidedNotRewarded,
}

impl CancelReason {
    /// Why the order was cancelled, for the Activity alert.
    pub fn describe(&self) -> &'static str {
        match self {
            CancelReason::DepthDropped => "depth ahead fell below your minimum",
            CancelReason::OtherSideDown => {
                "the other side has no live order, and below 10¢ or above 90¢ Polymarket only rewards both sides together"
            }
            CancelReason::OneSidedNotRewarded => {
                "below 10¢ or above 90¢ Polymarket only rewards both sides together, and only this side is farmed"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
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
/// `own` lists our own orders resting on this token (price, USD), this leg's
/// included: they are in the public book, but depth ahead must be other
/// people's money, or a re-peg could count its own old order as protection.
///
/// Does NOT perform any I/O — pure logic, returns an action for the caller to execute.
pub fn evaluate(
    config: &MarketConfig,
    book: &TokenBook,
    status: &OrderStatus,
    own: &[(Decimal, Decimal)],
) -> QuoteAction {
    // ── 1. Deactivate checks (highest priority) ────────────────────────────
    // Paused or expired with nothing to cancel (Idle): hold, so the alert fires
    // once, not on every book update. Cancelling: hold, its cancel is in flight
    // or retried by the timer (placer::confirm_stuck_cancel). A volatility trip
    // on an unpaused leg fires whatever the status, even with no order, so the
    // pause is saved; the leg then holds as paused.
    if matches!(status, OrderStatus::Cancelling { .. }) {
        if !config.paused && volatility_tripped(config, book) {
            return QuoteAction::Deactivate { order_id: live_order_id(status), reason: DeactivateReason::Volatility };
        }
        return QuoteAction::Hold;
    }
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
    if volatility_tripped(config, book) {
        // Idle too: holding would leave an unpaused leg waiting silently until
        // the price came back, which auto-pause promises not to do.
        return QuoteAction::Deactivate { order_id: live_order_id(status), reason: DeactivateReason::Volatility };
    }

    // ── 2. Skip while a placement is in flight (Cancelling held above) ─────
    if matches!(status, OrderStatus::Placing { .. }) {
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
        // Degenerate market — best_bid is too low to quote below it. Pull a
        // live order; with none there is nothing to cancel (sending a Cancel
        // with an empty id made a pointless API call and a false "Order
        // cancelled" alert on every book update).
        return match live_order_id(status) {
            Some(order_id) => QuoteAction::Cancel { order_id, reason: CancelReason::DepthDropped },
            None => QuoteAction::Hold,
        };
    }
    let target = TokenBook::snap_to_tick(raw_target, config.tick_size);

    match status {
        OrderStatus::Idle => {
            // Placing from scratch — check depth at the target we're about to use.
            let depth = book.others_depth_between(target, best_bid, own);
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
                let target_depth = book.others_depth_between(target, best_bid, own);
                if target_depth >= config.min_depth_between {
                    return QuoteAction::Replace { order_id: order_id.clone(), new_price: target };
                } else {
                    // Can't safely re-place either — cancel and wait.
                    return QuoteAction::Cancel { order_id: order_id.clone(), reason: CancelReason::DepthDropped };
                }
            }

            // ── Is our EXISTING order still protected? ─────────────────────
            // Check depth between the current order price and best_bid, not the
            // target: using the target would cancel a still-protected order just
            // because depth is thin at a position we haven't moved to yet.
            let current_depth = book.others_depth_between(*current_price, best_bid, own);
            if current_depth < config.min_depth_between {
                return QuoteAction::Cancel { order_id: order_id.clone(), reason: CancelReason::DepthDropped };
            }

            // ── Should we move to follow best_bid? ────────────────────────
            // best_bid moved away — target shifted. Only replace if the new position
            // is safe. If depth at target is thin, keep the existing protected order.
            if target != *current_price {
                let target_depth = book.others_depth_between(target, best_bid, own);
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

/// The best bid has moved at least `max_volatility` from where the leg started.
fn volatility_tripped(config: &MarketConfig, book: &TokenBook) -> bool {
    match (config.benchmark_bid, config.max_volatility, book.best_bid) {
        (Some(benchmark), Some(max_move), Some(bb)) => (bb - benchmark).abs() >= max_move,
        _ => false,
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

    /// The quoter on its own: no orders of ours in the book.
    fn ev(c: &MarketConfig, b: &TokenBook, s: &OrderStatus) -> QuoteAction {
        evaluate(c, b, s, &[])
    }

    /// The bug this guards against: best bid moves into our order, and the only
    /// "depth" between the new target and the best bid is our own old order. It
    /// must not count as protection — after the re-peg it is gone.
    #[test]
    fn our_own_order_is_not_depth_ahead() {
        let cfg = config(dec!(0.02), dec!(400)); // need $400 ahead
        let mut book = TokenBook { best_bid: Some(dec!(0.20)), best_ask: Some(dec!(0.21)), ..Default::default() };
        book.bids.insert(dec!(0.20), dec!(500)); // $100 of other people's bids
        book.bids.insert(dec!(0.19), dec!(2631.58)); // ~$500: our own resting order
        let live = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        // Counting our order ($600 ≥ $400) would re-peg to 0.18 with only $100 really ahead.
        assert!(matches!(ev(&cfg, &book, &live), QuoteAction::Replace { .. }), "old behaviour, for contrast");
        let own = [(dec!(0.19), dec!(500))];
        assert!(matches!(
            evaluate(&cfg, &book, &live, &own),
            QuoteAction::Cancel { reason: CancelReason::DepthDropped, .. }
        ));
        // With enough other people's money ahead, it still re-pegs.
        book.bids.insert(dec!(0.20), dec!(2500)); // $500 of others
        assert!(
            matches!(evaluate(&cfg, &book, &live, &own), QuoteAction::Replace { new_price, .. } if new_price == dec!(0.18))
        );
    }
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
        let action = ev(&cfg, &book, &OrderStatus::Idle);
        assert!(matches!(action, QuoteAction::Place { price } if price == dec!(0.19)));
    }

    #[test]
    fn holds_when_depth_insufficient() {
        let cfg = config(dec!(0.02), dec!(1000));
        let book = book_with_depth(dec!(0.21), dec!(50), dec!(0.02)); // only $50 depth
        let action = ev(&cfg, &book, &OrderStatus::Idle);
        assert_eq!(action, QuoteAction::Hold);
    }

    #[test]
    fn cancels_when_depth_drops_between_order_and_best_bid() {
        // best_bid=0.21, order at 0.19, gap=0.02 (ok), but depth between 0.19 and 0.21 is thin
        let cfg = config(dec!(0.02), dec!(500));
        let book = book_with_depth(dec!(0.21), dec!(50), dec!(0.02)); // only $50 depth at mid
        let status = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        let action = ev(&cfg, &book, &status);
        assert!(matches!(action, QuoteAction::Cancel { reason: CancelReason::DepthDropped, .. }));
    }

    #[test]
    fn replaces_when_best_bid_moves_into_order() {
        // best_bid moved to 0.20 — gap = 0.20 - 0.19 = 0.01 < distance 0.02 → must move farther out
        // book_with_depth places level at best_bid - distance/2 = 0.20 - 0.01 = 0.19
        // others_depth_between(target=0.18, best_bid=0.20) includes the level at 0.19 → $500 depth
        let cfg = config(dec!(0.02), dec!(100));
        let book = book_with_depth(dec!(0.20), dec!(500), dec!(0.02));
        let status = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        let action = ev(&cfg, &book, &status);
        assert!(matches!(action, QuoteAction::Replace { new_price, .. } if new_price == dec!(0.18)));
    }

    #[test]
    fn replaces_when_best_bid_moves_away() {
        // best_bid moved from 0.21 to 0.25 — our order at 0.19 is now too far out.
        // target = 0.23, depth at (0.23, 0.25] = $144, current_depth at (0.19, 0.25] = $276,
        // both above the $100 minimum.
        // Expects Replace to 0.23 (move closer to maintain spread).
        let cfg = config(dec!(0.02), dec!(100));
        // Place depth at 0.24 (between new target 0.23 and best_bid 0.25) and at 0.22 (between old 0.19 and 0.25)
        let mut book = TokenBook { best_bid: Some(dec!(0.25)), best_ask: Some(dec!(0.26)), ..Default::default() };
        book.bids.insert(dec!(0.24), dec!(600)); // $144 depth between 0.23 and 0.25
        book.bids.insert(dec!(0.22), dec!(600)); // also between 0.19 and 0.25
        let status = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        let action = ev(&cfg, &book, &status);
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
        let action = ev(&cfg, &book, &status);
        assert_eq!(action, QuoteAction::Hold);
    }

    #[test]
    fn expired_config_deactivates() {
        let mut cfg = config(dec!(0.02), dec!(100));
        cfg.expires_at = Utc::now() - Duration::seconds(1);
        let book = book_with_depth(dec!(0.21), dec!(500), dec!(0.02));
        let status = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.19) };
        let action = ev(&cfg, &book, &status);
        assert!(matches!(action, QuoteAction::Deactivate { reason: DeactivateReason::Expired, .. }));
    }

    #[test]
    fn a_market_too_low_to_quote_cancels_a_live_order_and_otherwise_holds() {
        // best bid 1¢ with a 2¢ distance: there is no price to quote at.
        let cfg = config(dec!(0.02), dec!(0));
        let book = TokenBook { best_bid: Some(dec!(0.01)), best_ask: Some(dec!(0.02)), ..Default::default() };
        let live = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.01) };
        assert_eq!(
            ev(&cfg, &book, &live),
            QuoteAction::Cancel { order_id: "ord1".to_string(), reason: CancelReason::DepthDropped }
        );
        assert_eq!(ev(&cfg, &book, &OrderStatus::Idle), QuoteAction::Hold, "no order, nothing to cancel");
        for status in [OrderStatus::Idle, OrderStatus::Placing { price: dec!(0.01) }] {
            assert!(
                !matches!(ev(&cfg, &book, &status), QuoteAction::Cancel { ref order_id, .. } if order_id.is_empty()),
                "never a Cancel with an empty order id ({status:?})"
            );
        }
    }

    #[test]
    fn a_leg_already_cancelling_holds_even_when_paused_expired_or_volatile() {
        let book = TokenBook { best_bid: Some(dec!(0.30)), best_ask: Some(dec!(0.31)), ..Default::default() };
        let cancelling = OrderStatus::Cancelling { order_id: "ord1".to_string(), since: std::time::Instant::now() };
        let mut paused = config(dec!(0.02), dec!(0));
        paused.paused = true;
        let mut expired = config(dec!(0.02), dec!(0));
        expired.expires_at = Utc::now() - Duration::seconds(1);
        let mut volatile = config(dec!(0.02), dec!(0));
        volatile.benchmark_bid = Some(dec!(0.20));
        volatile.max_volatility = Some(dec!(0.05));
        for cfg in [&paused, &expired] {
            assert_eq!(ev(cfg, &book, &cancelling), QuoteAction::Hold, "no repeat Deactivate while cancelling");
        }
        // A volatility trip still pauses the leg once (keeping the order being
        // cancelled); once paused it holds like the others.
        assert_eq!(
            ev(&volatile, &book, &cancelling),
            QuoteAction::Deactivate { order_id: Some("ord1".to_string()), reason: DeactivateReason::Volatility }
        );
        volatile.paused = true;
        assert_eq!(ev(&volatile, &book, &cancelling), QuoteAction::Hold);
        // With a live order they still deactivate, and a Placing leg is still reset.
        let live = OrderStatus::Live { order_id: "ord1".to_string(), price: dec!(0.28) };
        assert!(matches!(ev(&paused, &book, &live), QuoteAction::Deactivate { order_id: Some(_), .. }));
        assert!(matches!(
            ev(&paused, &book, &OrderStatus::Placing { price: dec!(0.28) }),
            QuoteAction::Deactivate { order_id: None, .. }
        ));
    }

    #[test]
    fn a_volatility_trip_pauses_a_leg_even_with_no_order() {
        let book = TokenBook { best_bid: Some(dec!(0.30)), best_ask: Some(dec!(0.31)), ..Default::default() };
        let mut cfg = config(dec!(0.02), dec!(0));
        cfg.benchmark_bid = Some(dec!(0.20));
        cfg.max_volatility = Some(dec!(0.05));
        // Waiting with no order: it pauses (and alerts) instead of holding silently.
        assert_eq!(
            ev(&cfg, &book, &OrderStatus::Idle),
            QuoteAction::Deactivate { order_id: None, reason: DeactivateReason::Volatility }
        );
        assert_eq!(
            ev(&cfg, &book, &OrderStatus::Placing { price: dec!(0.28) }),
            QuoteAction::Deactivate { order_id: None, reason: DeactivateReason::Volatility }
        );
        // Once paused it holds: one alert, not one per book update.
        cfg.paused = true;
        assert_eq!(ev(&cfg, &book, &OrderStatus::Idle), QuoteAction::Hold);
        // Inside the limit (exactly at it trips; just under doesn't).
        cfg.paused = false;
        cfg.benchmark_bid = Some(dec!(0.2501));
        assert!(!matches!(ev(&cfg, &book, &OrderStatus::Idle), QuoteAction::Deactivate { .. }));
        cfg.benchmark_bid = Some(dec!(0.25));
        assert!(matches!(ev(&cfg, &book, &OrderStatus::Idle), QuoteAction::Deactivate { .. }));
        // No limit or no reference: never trips.
        cfg.max_volatility = None;
        assert!(!matches!(ev(&cfg, &book, &OrderStatus::Idle), QuoteAction::Deactivate { .. }));
        // An expired leg with no order still just holds (expiry is checked first).
        let mut expired = config(dec!(0.02), dec!(0));
        expired.benchmark_bid = Some(dec!(0.20));
        expired.max_volatility = Some(dec!(0.05));
        expired.expires_at = Utc::now() - Duration::seconds(1);
        assert_eq!(ev(&expired, &book, &OrderStatus::Idle), QuoteAction::Hold);
    }
}
