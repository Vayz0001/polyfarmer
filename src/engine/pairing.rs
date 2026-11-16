//! Polymarket's two-sided rule, applied to quoting.
//!
//! While a market's midpoint is under 10¢ or over 90¢, Polymarket only rewards
//! liquidity on *both* outcomes: a bid on one side alone scores nothing but can
//! still be filled. Each leg is quoted on its own by [`quoter::evaluate`]; this
//! module looks at the market's legs together and, in that range only:
//!
//! * lets a leg **place** only when the other side is up too (live, being
//!   placed, or placing in the same pass), so both go up together;
//! * **pulls** a leg that has been up alone (no live order on the other side)
//!   for [`PAIR_GRACE`], which rides out re-pegs and placements in flight. Its
//!   own cancels and re-places don't restart that clock;
//! * after a pull, holds that leg back for a growing delay (30 s doubling to
//!   10 min, reset once both sides are live together or the midpoint is well
//!   inside 10–90¢), so a side that keeps failing can't drive a
//!   place-and-cancel loop;
//! * keeps a leg farmed without the other outcome off the book.
//!
//! Inside 10–90¢ one side alone still earns a third of the score and quoting is
//! unchanged. Near the edges (10–11¢ and 89–90¢), or with no midpoint, the rule
//! neither acts nor forgets: a price flickering across 90¢ can't keep resetting
//! it. Pure decision logic: no I/O.

use crate::engine::quoter::{self, CancelReason, DeactivateReason, QuoteAction};
use crate::engine::ws_manager::AppState;
use crate::rewards::market_data::single_sided_allowed;
use crate::types::{MarketConfig, OrderStatus};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::Instant;

/// How long a leg may stay up without the other side before it is pulled.
pub const PAIR_GRACE: Duration = Duration::from_secs(30);
const PAIR_BACKOFF_BASE: Duration = Duration::from_secs(30);
const PAIR_BACKOFF_MAX: Duration = Duration::from_secs(600);
/// A grace clock not refreshed for this long is stale: the leg has been off the
/// book (or the rule out of force) long enough for a fresh start. Twice the
/// grace period, so the 30 s fallback timer alone still pulls a lone leg.
const PAIR_STALE: Duration = Duration::from_secs(60);
/// Midpoints in this range are "well inside": the rule's state is cleared.
const CLEARLY_INSIDE: (Decimal, Decimal) = (dec!(0.11), dec!(0.89));

/// Why a leg is held off the book by the two-sided rule (dashboard display).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairWait {
    /// The other outcome is farmed but isn't up.
    OtherSide,
    /// Only this outcome is farmed.
    NoOtherSide,
}

/// Grace clock of a leg that has been up without the other side.
#[derive(Debug, Clone, Copy)]
pub struct AloneClock {
    /// When it first went up alone.
    pub since: Instant,
    /// When it was last seen with an order (still alone).
    pub last: Instant,
    /// Started by a Place decision, not yet seen with an order: renewed by the
    /// next decision, so a placement that never happened costs no grace.
    pub pending: bool,
}

/// `(config id, action, token id, order size, tick size)`, as the engine's
/// action runners take them.
pub type Decision = (String, QuoteAction, String, Decimal, Decimal);

/// Evaluate the selected legs and apply the two-sided rule. Updates the rule's
/// bookkeeping in `s` (grace clocks, back-off, display reasons). Legs without a
/// book yet are skipped, as before.
pub fn decide(s: &mut AppState, select: impl Fn(&MarketConfig) -> bool, now: Instant) -> Vec<Decision> {
    // Drop bookkeeping for removed legs.
    let ids: HashSet<String> = s.configs.iter().map(|c| c.id.clone()).collect();
    s.pair_alone_since.retain(|id, _| ids.contains(id));
    s.pair_pulls.retain(|id, _| ids.contains(id));
    s.pair_retry_at.retain(|id, _| ids.contains(id));
    s.pair_wait.retain(|id, _| ids.contains(id));

    let mut out = Vec::new();
    for i in 0..s.configs.len() {
        if !select(&s.configs[i]) {
            continue;
        }
        let c = &s.configs[i];
        let Some(book) = s.books.get(&c.token_id) else {
            let id = c.id.clone();
            s.pair_wait.remove(&id);
            continue;
        };
        let status = s.order_status.get(&c.id).cloned().unwrap_or_default();
        let raw = quoter::evaluate(c, book, &status);
        let (id, token, size, tick) = (c.id.clone(), c.token_id.clone(), c.order_size, c.tick_size);
        let action = gate(s, i, &status, raw, now);
        if let QuoteAction::Deactivate { order_id, reason: DeactivateReason::Volatility } = &action {
            // Record the auto-pause now, under the lock (the runner saves it),
            // so the timer and the feed can't both fire it: a racing evaluation
            // sees a paused leg with a cancelling order, or none, and holds.
            s.configs[i].paused = true;
            match (&status, order_id) {
                (OrderStatus::Live { .. }, Some(oid)) => {
                    s.order_status.insert(
                        id.clone(),
                        OrderStatus::Cancelling { order_id: oid.clone(), since: std::time::Instant::now() },
                    );
                }
                // A placement in flight finds it no longer owns the leg and
                // cancels the order it gets back (placer::run_place).
                (OrderStatus::Placing { .. }, _) => {
                    s.order_status.insert(id.clone(), OrderStatus::Idle);
                }
                _ => {}
            }
        }
        out.push((id, action, token, size, tick));
    }
    out
}

/// The legs farming the other outcome of `c`'s market.
fn partners<'a>(s: &'a AppState, c: &MarketConfig) -> impl Iterator<Item = &'a MarketConfig> {
    let (cid, token) = (c.condition_id.clone(), c.token_id.clone());
    s.configs
        .iter()
        .filter(move |p| !cid.is_empty() && p.condition_id.eq_ignore_ascii_case(&cid) && p.token_id != token)
}

/// The market's midpoint from this leg's book, or else a partner's (the two
/// outcomes' books mirror each other, and 10–90¢ is symmetric).
fn midpoint(s: &AppState, c: &MarketConfig) -> Option<Decimal> {
    let mid = |token: &str| {
        let b = s.books.get(token)?;
        Some((b.best_bid? + b.best_ask?) / Decimal::TWO)
    };
    mid(&c.token_id).or_else(|| partners(s, c).find_map(|p| mid(&p.token_id)))
}

/// Held back after a pull (its back-off hasn't run out).
fn held_back(s: &AppState, id: &str, now: Instant) -> bool {
    s.pair_retry_at.get(id).is_some_and(|at| now < *at)
}

/// Alone past the grace period (and not since reset by a whole grace period
/// off the book).
fn overdue(s: &AppState, id: &str, now: Instant) -> bool {
    s.pair_alone_since.get(id).is_some_and(|c| {
        !c.pending
            && now.saturating_duration_since(c.since) >= PAIR_GRACE
            && now.saturating_duration_since(c.last) < PAIR_STALE
    })
}

/// A partner with no order that would place one now — on its own terms and
/// under this rule (not held back, not overdue).
fn could_place(s: &AppState, p: &MarketConfig, now: Instant) -> bool {
    let idle = matches!(s.order_status.get(&p.id), None | Some(OrderStatus::Idle));
    idle && !s.heartbeat_paused
        && !s.place_retry_at.get(&p.id).is_some_and(|at| now < *at)
        && !held_back(s, &p.id, now)
        && !overdue(s, &p.id, now)
        && s.books
            .get(&p.token_id)
            .is_some_and(|b| matches!(quoter::evaluate(p, b, &OrderStatus::Idle), QuoteAction::Place { .. }))
}

fn backoff(pulls: u32) -> Duration {
    let shift = pulls.saturating_sub(1).min(16);
    PAIR_BACKOFF_BASE.saturating_mul(1u32 << shift).min(PAIR_BACKOFF_MAX)
}

/// Forget the rule's state for a leg (the rule doesn't apply to it now).
fn reset(s: &mut AppState, id: &str) {
    s.pair_alone_since.remove(id);
    s.pair_pulls.remove(id);
    s.pair_retry_at.remove(id);
    s.pair_wait.remove(id);
}

/// Book a pull: count it, start the back-off, and say why the leg waits.
fn pull(s: &mut AppState, id: &str, wait: PairWait, now: Instant) {
    s.pair_alone_since.remove(id);
    let pulls = s.pair_pulls.entry(id.to_string()).or_insert(0);
    *pulls = pulls.saturating_add(1);
    let until = now + backoff(*pulls);
    s.pair_retry_at.insert(id.to_string(), until);
    s.pair_wait.insert(id.to_string(), wait);
}

fn gate(s: &mut AppState, i: usize, status: &OrderStatus, raw: QuoteAction, now: Instant) -> QuoteAction {
    let c = &s.configs[i];
    let id = c.id.clone();
    let mid = midpoint(s, c);

    let mut has_partner = false;
    let (mut partner_live, mut partner_ready) = (false, false);
    for p in partners(s, c) {
        has_partner = true;
        match s.order_status.get(&p.id) {
            Some(OrderStatus::Live { .. }) => {
                partner_live = true;
                partner_ready = true;
            }
            Some(OrderStatus::Placing { .. }) => partner_ready = true,
            _ => partner_ready |= could_place(s, p, now),
        }
    }
    let wait = if has_partner { PairWait::OtherSide } else { PairWait::NoOtherSide };

    if mid.is_some_and(|m| m >= CLEARLY_INSIDE.0 && m <= CLEARLY_INSIDE.1) {
        reset(s, &id);
        return raw;
    }
    if partner_live {
        // The other side is live: the rule is satisfied (and it is fine to join
        // it even during a back-off, which only guards against a failing side).
        s.pair_alone_since.remove(&id);
        s.pair_wait.remove(&id);
        if matches!(status, OrderStatus::Live { .. }) {
            s.pair_pulls.remove(&id);
            s.pair_retry_at.remove(&id);
        }
        return raw;
    }
    let two_sided_only = mid.is_some_and(|m| !single_sided_allowed(m));
    if !two_sided_only {
        // Near the edges, or no midpoint: one side may rest (or we can't tell),
        // so quote as usual — but keep the clock and honour a back-off.
        if matches!(raw, QuoteAction::Place { .. }) && held_back(s, &id, now) {
            s.pair_wait.insert(id, wait);
            return QuoteAction::Hold;
        }
        s.pair_wait.remove(&id);
        return raw;
    }

    // Outside 10–90¢. The grace clock runs while this leg has an order and the
    // other side has none live. A stale clock (off the book, or the rule out of
    // force, for a while) is a fresh start; so is one only a decision started.
    if s.pair_alone_since.get(&id).is_some_and(|c| {
        now.saturating_duration_since(c.last) >= PAIR_STALE || (c.pending && matches!(status, OrderStatus::Idle))
    }) {
        s.pair_alone_since.remove(&id);
    }
    match status {
        OrderStatus::Live { .. } | OrderStatus::Placing { .. } => {
            let clock =
                s.pair_alone_since.entry(id.clone()).or_insert(AloneClock { since: now, last: now, pending: false });
            clock.last = now;
            clock.pending = false;
        }
        // Cancelling (its own cancel, a pull, a pause): keeps a running clock
        // going but never starts one.
        OrderStatus::Cancelling { .. } => {
            if let Some(clock) = s.pair_alone_since.get_mut(&id) {
                clock.last = now;
            }
        }
        OrderStatus::Idle => {}
    }
    let late = overdue(s, &id, now);

    match status {
        OrderStatus::Idle => match raw {
            QuoteAction::Place { .. } if late => {
                // Alone past the grace period already (it cancelled on its own
                // and wants back in): held back as if pulled.
                pull(s, &id, wait, now);
                QuoteAction::Hold
            }
            QuoteAction::Place { .. } if !partner_ready || held_back(s, &id, now) => {
                if s.heartbeat_paused {
                    s.pair_wait.remove(&id); // the heartbeat holds everything; not this rule
                } else {
                    s.pair_wait.insert(id, wait);
                }
                QuoteAction::Hold
            }
            QuoteAction::Place { price } => {
                // Going up while the other side isn't live yet: the clock starts now.
                s.pair_wait.remove(&id);
                s.pair_alone_since.entry(id).or_insert(AloneClock { since: now, last: now, pending: true });
                QuoteAction::Place { price }
            }
            other => {
                s.pair_wait.remove(&id);
                other
            }
        },
        OrderStatus::Live { order_id, .. } => {
            if !late || matches!(raw, QuoteAction::Cancel { .. } | QuoteAction::Deactivate { .. }) {
                return raw;
            }
            pull(s, &id, wait, now);
            // Marked Cancelling right away (as the action runners will), so an
            // evaluation racing this one sees the cancel and holds.
            s.order_status
                .insert(id, OrderStatus::Cancelling { order_id: order_id.clone(), since: std::time::Instant::now() });
            let reason = if has_partner { CancelReason::OtherSideDown } else { CancelReason::OneSidedNotRewarded };
            QuoteAction::Cancel { order_id: order_id.clone(), reason }
        }
        // An order in flight: its outcome decides; the clock keeps running.
        OrderStatus::Placing { .. } | OrderStatus::Cancelling { .. } => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::orderbook::TokenBook;
    use crate::engine::placer;
    use chrono::Utc;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use std::path::PathBuf;

    const CID: &str = "0xMarket";

    fn leg(id: &str, token: &str, cid: &str) -> MarketConfig {
        MarketConfig {
            id: id.to_string(),
            url: String::new(),
            label: format!("leg {id}"),
            condition_id: cid.to_string(),
            token_id: token.to_string(),
            token_label: token.to_string(),
            tick_size: dec!(0.01),
            distance: dec!(0.01),
            min_depth_between: dec!(10),
            order_size: dec!(50),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            paused: false,
            benchmark_bid: None,
            max_volatility: None,
        }
    }

    /// A book with best bid/ask around `mid`, and `depth` USD between the
    /// best bid and one tick below it (where a 1¢-distance leg quotes).
    fn book(bid: Decimal, ask: Decimal, depth: Decimal) -> TokenBook {
        let mut b = TokenBook { best_bid: Some(bid), best_ask: Some(ask), ..Default::default() };
        if depth > dec!(0) {
            b.bids.insert(bid, depth / bid);
        }
        b
    }

    /// Yes at 95–96¢ (two-sided only), No mirrored at 4–5¢.
    fn state(yes_depth: Decimal, no_depth: Decimal) -> AppState {
        let mut s = AppState::new(PathBuf::from("unused.json"));
        s.configs = vec![leg("y", "YES", CID), leg("n", "NO", CID)];
        s.books.insert("YES".into(), book(dec!(0.95), dec!(0.96), yes_depth));
        s.books.insert("NO".into(), book(dec!(0.04), dec!(0.05), no_depth));
        s
    }

    fn live(s: &mut AppState, id: &str, price: Decimal) {
        s.order_status.insert(id.into(), OrderStatus::Live { order_id: format!("ord-{id}"), price });
    }

    fn action_of(d: &[Decision], id: &str) -> QuoteAction {
        d.iter().find(|x| x.0 == id).map(|x| x.1.clone()).expect("leg evaluated")
    }

    fn all(s: &mut AppState, now: Instant) -> Vec<Decision> {
        decide(s, |_| true, now)
    }

    /// An auto-pause is recorded when it is decided, so a second evaluation
    /// (the timer racing the feed) holds instead of alerting and cancelling again.
    #[test]
    fn an_auto_pause_is_decided_once() {
        for live_order in [true, false] {
            let mut s = AppState::new(PathBuf::from("unused.json"));
            let mut c = leg("y", "YES", CID);
            c.benchmark_bid = Some(dec!(0.40));
            c.max_volatility = Some(dec!(0.05));
            s.configs = vec![c];
            s.books.insert("YES".into(), book(dec!(0.50), dec!(0.51), dec!(100)));
            if live_order {
                live(&mut s, "y", dec!(0.49));
            }
            let now = Instant::now();
            let first = action_of(&all(&mut s, now), "y");
            let QuoteAction::Deactivate { order_id, reason: DeactivateReason::Volatility } = first else {
                panic!("expected an auto-pause, got {first:?}");
            };
            assert_eq!(order_id.is_some(), live_order);
            assert!(s.configs[0].paused, "paused at decision time");
            if live_order {
                assert!(matches!(s.order_status.get("y"), Some(OrderStatus::Cancelling { .. })));
            }
            assert_eq!(action_of(&all(&mut s, now), "y"), QuoteAction::Hold, "no second alert or cancel");
        }
    }

    /// A trip while an order is being placed: the placement no longer owns the
    /// leg (so it cancels what it gets back), and a racing evaluation holds
    /// rather than sending a second, "Paused", alert.
    #[test]
    fn an_auto_pause_during_a_placement_releases_it() {
        let mut s = AppState::new(PathBuf::from("unused.json"));
        let mut c = leg("y", "YES", CID);
        c.benchmark_bid = Some(dec!(0.40));
        c.max_volatility = Some(dec!(0.05));
        s.configs = vec![c];
        s.books.insert("YES".into(), book(dec!(0.50), dec!(0.51), dec!(100)));
        let now = Instant::now();
        placer::begin_place(&mut s, "y", dec!(0.49), now).expect("slot claimed");
        assert!(matches!(s.order_status.get("y"), Some(OrderStatus::Placing { .. })));
        assert_eq!(
            action_of(&all(&mut s, now), "y"),
            QuoteAction::Deactivate { order_id: None, reason: DeactivateReason::Volatility }
        );
        assert!(s.configs[0].paused);
        assert!(matches!(s.order_status.get("y"), Some(OrderStatus::Idle)), "placement released");
        assert_eq!(action_of(&all(&mut s, now), "y"), QuoteAction::Hold, "no second alert");
    }

    #[test]
    fn both_sides_ready_place_together() {
        let mut s = state(dec!(100), dec!(100));
        let d = all(&mut s, Instant::now());
        assert!(matches!(action_of(&d, "y"), QuoteAction::Place { .. }));
        assert!(matches!(action_of(&d, "n"), QuoteAction::Place { .. }));
        assert!(s.pair_wait.is_empty());
    }

    #[test]
    fn a_side_that_can_place_waits_for_the_other() {
        // No's depth is thin: it waits on its own terms, so Yes waits for it.
        let mut s = state(dec!(100), dec!(0));
        let d = all(&mut s, Instant::now());
        assert_eq!(action_of(&d, "y"), QuoteAction::Hold);
        assert_eq!(action_of(&d, "n"), QuoteAction::Hold);
        assert_eq!(s.pair_wait.get("y"), Some(&PairWait::OtherSide));
        assert_eq!(s.pair_wait.get("n"), None, "No waits for its own depth, not for Yes");
    }

    #[test]
    fn a_side_placing_or_live_lets_the_other_place() {
        for other in
            [OrderStatus::Placing { price: dec!(0.03) }, OrderStatus::Live { order_id: "o".into(), price: dec!(0.03) }]
        {
            let mut s = state(dec!(100), dec!(0));
            s.order_status.insert("n".into(), other.clone());
            let d = all(&mut s, Instant::now());
            assert!(matches!(action_of(&d, "y"), QuoteAction::Place { .. }), "{other:?}");
        }
    }

    #[test]
    fn a_lone_live_leg_is_pulled_after_the_grace_period_and_backs_off() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        assert_eq!(action_of(&all(&mut s, t0), "y"), QuoteAction::Hold, "alone, but inside the grace period");
        let almost = t0 + PAIR_GRACE - Duration::from_millis(1);
        assert_eq!(action_of(&all(&mut s, almost), "y"), QuoteAction::Hold);
        let t1 = t0 + PAIR_GRACE;
        assert_eq!(
            action_of(&all(&mut s, t1), "y"),
            QuoteAction::Cancel { order_id: "ord-y".into(), reason: CancelReason::OtherSideDown }
        );
        assert_eq!(s.pair_wait.get("y"), Some(&PairWait::OtherSide));
        assert_eq!(s.pair_retry_at.get("y"), Some(&(t1 + Duration::from_secs(30))));
        assert!(!s.place_retry_at.contains_key("y"), "placement back-off untouched");
        assert!(matches!(s.order_status.get("y"), Some(OrderStatus::Cancelling { .. })), "marked at once");

        // Cancelled; No becomes placeable, but Yes is held back for 30s, so both wait.
        s.order_status.insert("y".into(), OrderStatus::Idle);
        s.books.insert("NO".into(), book(dec!(0.04), dec!(0.05), dec!(100)));
        let d = all(&mut s, t1 + Duration::from_secs(1));
        assert_eq!(action_of(&d, "y"), QuoteAction::Hold);
        assert_eq!(action_of(&d, "n"), QuoteAction::Hold, "Yes is held back, so No waits too");
        // After the back-off both go up together.
        let d = all(&mut s, t1 + Duration::from_secs(30));
        assert!(matches!(action_of(&d, "y"), QuoteAction::Place { .. }));
        assert!(matches!(action_of(&d, "n"), QuoteAction::Place { .. }));
    }

    #[test]
    fn back_off_doubles_up_to_ten_minutes_and_resets_once_both_sides_are_live() {
        assert_eq!(backoff(1), Duration::from_secs(30));
        assert_eq!(backoff(2), Duration::from_secs(60));
        assert_eq!(backoff(5), Duration::from_secs(480));
        assert_eq!(backoff(6), Duration::from_secs(600));
        assert_eq!(backoff(u32::MAX), Duration::from_secs(600));

        let mut s = state(dec!(100), dec!(100));
        s.pair_pulls.insert("y".into(), 4);
        live(&mut s, "y", dec!(0.94));
        live(&mut s, "n", dec!(0.03));
        all(&mut s, Instant::now());
        assert!(!s.pair_pulls.contains_key("y"));
    }

    #[test]
    fn re_pegging_does_not_restart_the_grace_clock() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        // A re-peg: Cancelling, then Placing, then Live again.
        s.order_status
            .insert("y".into(), OrderStatus::Cancelling { order_id: "ord-y".into(), since: std::time::Instant::now() });
        all(&mut s, t0 + Duration::from_secs(10));
        s.order_status.insert("y".into(), OrderStatus::Placing { price: dec!(0.94) });
        all(&mut s, t0 + Duration::from_secs(12));
        live(&mut s, "y", dec!(0.94));
        assert!(matches!(action_of(&all(&mut s, t0 + PAIR_GRACE), "y"), QuoteAction::Cancel { .. }));
    }

    #[test]
    fn its_own_cancels_and_re_places_do_not_restart_the_clock() {
        // The other side can place on paper but never gets live (say its
        // orders keep bouncing for lack of balance); this side's depth flickers,
        // so it cancels and re-places on its own. It must still come off.
        let mut s = state(dec!(100), dec!(100));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        s.order_status
            .insert("y".into(), OrderStatus::Cancelling { order_id: "ord-y".into(), since: std::time::Instant::now() });
        all(&mut s, t0 + Duration::from_secs(20));
        s.order_status.insert("y".into(), OrderStatus::Idle);
        assert!(matches!(action_of(&all(&mut s, t0 + Duration::from_secs(21)), "y"), QuoteAction::Place { .. }));
        live(&mut s, "y", dec!(0.94));
        assert!(
            matches!(action_of(&all(&mut s, t0 + PAIR_GRACE), "y"), QuoteAction::Cancel { .. }),
            "30s after it first went up alone"
        );

        // Same, but it is idle when the grace period runs out: held back instead.
        let mut s = state(dec!(100), dec!(100));
        live(&mut s, "y", dec!(0.94));
        all(&mut s, t0);
        all(&mut s, t0 + Duration::from_secs(25));
        s.order_status.insert("y".into(), OrderStatus::Idle);
        assert_eq!(action_of(&all(&mut s, t0 + PAIR_GRACE), "y"), QuoteAction::Hold);
        assert_eq!(s.pair_pulls.get("y"), Some(&1));
        assert_eq!(s.pair_retry_at.get("y"), Some(&(t0 + PAIR_GRACE + Duration::from_secs(30))));
    }

    #[test]
    fn a_while_off_the_book_is_a_fresh_start() {
        let mut s = state(dec!(100), dec!(100));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0 + Duration::from_secs(10));
        s.order_status.insert("y".into(), OrderStatus::Idle);
        // Off the book from t0+10; at t0+70 that is PAIR_STALE.
        let d = all(&mut s, t0 + Duration::from_secs(70));
        assert!(matches!(action_of(&d, "y"), QuoteAction::Place { .. }));
        // A new clock from this placement, not the old one.
        assert_eq!(s.pair_alone_since.get("y").map(|c| c.since), Some(t0 + Duration::from_secs(70)));
        assert!(!s.pair_pulls.contains_key("y"));
    }

    #[test]
    fn the_fallback_timer_alone_still_pulls_a_lone_leg() {
        // A quiet market: evaluations only every 30 s.
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        assert!(matches!(action_of(&all(&mut s, t0 + Duration::from_secs(30)), "y"), QuoteAction::Cancel { .. }));
    }

    #[test]
    fn a_placement_that_never_happened_costs_no_grace() {
        // Yes keeps being told to place but the placer refuses (its own
        // failure back-off) until t+29; then both sides go up.
        let mut s = state(dec!(100), dec!(100));
        let t = Instant::now();
        for secs in 0..29 {
            let d = all(&mut s, t + Duration::from_secs(secs));
            assert!(matches!(action_of(&d, "y"), QuoteAction::Place { .. }));
            // refused: stays Idle
        }
        all(&mut s, t + Duration::from_secs(29));
        live(&mut s, "y", dec!(0.94));
        s.order_status.insert("n".into(), OrderStatus::Placing { price: dec!(0.03) });
        assert_eq!(
            action_of(&all(&mut s, t + Duration::from_millis(30_500)), "y"),
            QuoteAction::Hold,
            "a full grace period from when it actually went up"
        );
        assert!(matches!(action_of(&all(&mut s, t + Duration::from_secs(59)), "y"), QuoteAction::Cancel { .. }));
    }

    #[test]
    fn a_clock_left_from_before_a_long_spell_at_the_edge_restarts() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        all(&mut s, t0 + Duration::from_secs(20));
        // An hour at 89.5¢ (one side allowed): the rule stands aside.
        s.books.insert("YES".into(), book(dec!(0.89), dec!(0.90), dec!(100)));
        all(&mut s, t0 + Duration::from_secs(1800));
        // Back over 90¢ with No being placed: the full grace again, not an instant pull.
        s.books.insert("YES".into(), book(dec!(0.95), dec!(0.96), dec!(100)));
        s.order_status.insert("n".into(), OrderStatus::Placing { price: dec!(0.03) });
        let back = t0 + Duration::from_secs(3600);
        assert_eq!(action_of(&all(&mut s, back), "y"), QuoteAction::Hold);
        assert!(matches!(action_of(&all(&mut s, back + PAIR_GRACE), "y"), QuoteAction::Cancel { .. }));
    }

    #[test]
    fn a_back_off_holding_a_leg_at_the_edge_says_why() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        all(&mut s, t0 + PAIR_GRACE);
        s.order_status.insert("y".into(), OrderStatus::Idle);
        s.books.insert("YES".into(), book(dec!(0.89), dec!(0.90), dec!(100)));
        assert_eq!(action_of(&all(&mut s, t0 + PAIR_GRACE + Duration::from_secs(5)), "y"), QuoteAction::Hold);
        assert_eq!(s.pair_wait.get("y"), Some(&PairWait::OtherSide));
    }

    #[test]
    fn a_pull_leaves_the_placement_back_off_alone() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(3600);
        s.place_retry_at.insert("y".into(), later);
        all(&mut s, t0);
        all(&mut s, t0 + PAIR_GRACE);
        assert_eq!(s.place_retry_at.get("y"), Some(&later));
        assert!(s.pair_retry_at.contains_key("y"));
    }

    #[test]
    fn a_side_held_back_or_overdue_does_not_count_as_ready() {
        // No was up alone from t0 (Yes had no depth), cancelled itself at t0+20.
        let mut s = state(dec!(0), dec!(100));
        live(&mut s, "n", dec!(0.03));
        let t0 = Instant::now();
        all(&mut s, t0);
        s.order_status
            .insert("n".into(), OrderStatus::Cancelling { order_id: "ord-n".into(), since: std::time::Instant::now() });
        all(&mut s, t0 + Duration::from_secs(20));
        s.order_status.insert("n".into(), OrderStatus::Idle);
        // At t0+31 both have depth. No is overdue, so it is held back — and Yes
        // must not go up alone on the strength of No "being able to place".
        s.books.insert("YES".into(), book(dec!(0.95), dec!(0.96), dec!(100)));
        let d = all(&mut s, t0 + Duration::from_secs(31));
        assert_eq!(action_of(&d, "n"), QuoteAction::Hold);
        assert_eq!(action_of(&d, "y"), QuoteAction::Hold);
        // Held back No still doesn't count while its back-off runs.
        let d = all(&mut s, t0 + Duration::from_secs(40));
        assert_eq!(action_of(&d, "y"), QuoteAction::Hold);
        // Afterwards both go up together.
        let d = all(&mut s, t0 + Duration::from_secs(61));
        assert!(matches!(action_of(&d, "y"), QuoteAction::Place { .. }));
        assert!(matches!(action_of(&d, "n"), QuoteAction::Place { .. }));
    }

    #[test]
    fn a_pulled_leg_cancelling_does_not_start_a_new_clock() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        let t1 = t0 + PAIR_GRACE;
        assert!(matches!(action_of(&all(&mut s, t1), "y"), QuoteAction::Cancel { .. }));
        // The pull's cancel takes a while (or gets stuck for a bit).
        for ms in [200, 1500, 20_000] {
            all(&mut s, t1 + Duration::from_millis(ms));
        }
        assert!(!s.pair_alone_since.contains_key("y"));
        s.order_status.insert("y".into(), OrderStatus::Idle);
        s.books.insert("NO".into(), book(dec!(0.04), dec!(0.05), dec!(100)));
        let d = all(&mut s, t1 + Duration::from_millis(30_300));
        assert!(matches!(action_of(&d, "y"), QuoteAction::Place { .. }), "back-off over: no second pull");
        assert_eq!(s.pair_pulls.get("y"), Some(&1));
    }

    #[test]
    fn well_inside_ten_to_ninety_clears_the_back_off() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        all(&mut s, t0 + PAIR_GRACE);
        s.order_status.insert("y".into(), OrderStatus::Idle);
        s.books.insert("YES".into(), book(dec!(0.50), dec!(0.51), dec!(100)));
        let d = all(&mut s, t0 + PAIR_GRACE + Duration::from_secs(1));
        assert!(matches!(action_of(&d, "y"), QuoteAction::Place { .. }), "one side alone is fine at 50¢");
        assert!(s.pair_retry_at.is_empty() && s.pair_pulls.is_empty() && s.pair_wait.is_empty());
    }

    #[test]
    fn a_price_flickering_across_ninety_cents_still_gets_a_lone_leg_pulled() {
        // 1¢ tick, 1¢ spread: the midpoint flips between 90.5¢ and 89.5¢.
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.89));
        let t0 = Instant::now();
        let out = || book(dec!(0.90), dec!(0.91), dec!(100));
        let edge = || book(dec!(0.89), dec!(0.90), dec!(100));
        for (secs, b) in [(0, out()), (8, edge()), (16, out()), (24, edge())] {
            s.books.insert("YES".into(), b);
            assert!(!matches!(
                action_of(&all(&mut s, t0 + Duration::from_secs(secs)), "y"),
                QuoteAction::Cancel { .. }
            ));
        }
        s.books.insert("YES".into(), out());
        assert!(matches!(
            action_of(&all(&mut s, t0 + Duration::from_secs(31)), "y"),
            QuoteAction::Cancel { reason: CancelReason::OtherSideDown, .. }
        ));
        // And while held back, an edge reading doesn't let it straight back in.
        s.order_status.insert("y".into(), OrderStatus::Idle);
        s.books.insert("YES".into(), edge());
        assert_eq!(action_of(&all(&mut s, t0 + Duration::from_secs(35)), "y"), QuoteAction::Hold);
    }

    #[test]
    fn no_midpoint_for_a_while_neither_acts_nor_forgets() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        s.books.get_mut("YES").unwrap().best_ask = None;
        s.books.get_mut("NO").unwrap().best_ask = None;
        assert_eq!(action_of(&all(&mut s, t0 + Duration::from_secs(20)), "y"), QuoteAction::Hold);
        s.books.insert("YES".into(), book(dec!(0.95), dec!(0.96), dec!(100)));
        s.books.insert("NO".into(), book(dec!(0.04), dec!(0.05), dec!(0)));
        assert!(matches!(action_of(&all(&mut s, t0 + PAIR_GRACE), "y"), QuoteAction::Cancel { .. }));
    }

    #[test]
    fn an_evaluation_racing_a_pull_sees_the_cancel() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        assert!(matches!(action_of(&all(&mut s, t0 + PAIR_GRACE), "y"), QuoteAction::Cancel { .. }));
        // Before the runner gets to it, the best bid moves into the order: a
        // second evaluation must not re-peg (re-place) the leg being pulled.
        s.books.insert("YES".into(), book(dec!(0.94), dec!(0.95), dec!(100)));
        assert_eq!(action_of(&all(&mut s, t0 + PAIR_GRACE), "y"), QuoteAction::Hold);
    }

    #[test]
    fn a_leg_without_a_book_drops_its_waiting_reason() {
        let mut s = state(dec!(100), dec!(0));
        all(&mut s, Instant::now());
        assert!(s.pair_wait.contains_key("y"));
        s.books.remove("YES");
        all(&mut s, Instant::now());
        assert!(!s.pair_wait.contains_key("y"));
    }

    #[test]
    fn the_other_side_going_live_stops_the_clock() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        live(&mut s, "n", dec!(0.03));
        all(&mut s, t0 + Duration::from_secs(20));
        s.order_status.insert("n".into(), OrderStatus::Idle);
        // Alone again from t0+40: a fresh 30s.
        assert_eq!(action_of(&all(&mut s, t0 + Duration::from_secs(40)), "y"), QuoteAction::Hold);
        assert_eq!(action_of(&all(&mut s, t0 + Duration::from_secs(69)), "y"), QuoteAction::Hold);
        assert!(matches!(action_of(&all(&mut s, t0 + Duration::from_secs(70)), "y"), QuoteAction::Cancel { .. }));
    }

    #[test]
    fn a_paused_expired_or_failing_other_side_leaves_this_side_waiting() {
        let now = Instant::now();
        let mut paused = state(dec!(100), dec!(100));
        paused.configs[1].paused = true;
        let mut expired = state(dec!(100), dec!(100));
        expired.configs[1].expires_at = Utc::now() - chrono::Duration::seconds(1);
        let mut retrying = state(dec!(100), dec!(100));
        retrying.place_retry_at.insert("n".into(), now + Duration::from_secs(5));
        let mut no_book = state(dec!(100), dec!(100));
        no_book.books.remove("NO");
        let mut cancelling = state(dec!(100), dec!(100));
        cancelling
            .order_status
            .insert("n".into(), OrderStatus::Cancelling { order_id: "x".into(), since: std::time::Instant::now() });
        for (name, mut s) in [
            ("paused", paused),
            ("expired", expired),
            ("retrying", retrying),
            ("no book", no_book),
            ("cancelling", cancelling),
        ] {
            let d = all(&mut s, now);
            assert_eq!(action_of(&d, "y"), QuoteAction::Hold, "{name}");
            assert_eq!(s.pair_wait.get("y"), Some(&PairWait::OtherSide), "{name}");
        }
    }

    #[test]
    fn a_single_outcome_farm_stays_off_the_book_while_two_sided_only() {
        let mut s = state(dec!(100), dec!(100));
        s.configs.retain(|c| c.id == "y");
        let t0 = Instant::now();
        assert_eq!(action_of(&all(&mut s, t0), "y"), QuoteAction::Hold);
        assert_eq!(s.pair_wait.get("y"), Some(&PairWait::NoOtherSide));
        live(&mut s, "y", dec!(0.94));
        all(&mut s, t0);
        assert_eq!(
            action_of(&all(&mut s, t0 + PAIR_GRACE), "y"),
            QuoteAction::Cancel { order_id: "ord-y".into(), reason: CancelReason::OneSidedNotRewarded }
        );
    }

    #[test]
    fn inside_ten_to_ninety_cents_nothing_changes() {
        let mut s = state(dec!(100), dec!(0));
        s.books.insert("YES".into(), book(dec!(0.50), dec!(0.51), dec!(100)));
        s.books.insert("NO".into(), book(dec!(0.49), dec!(0.50), dec!(0)));
        let t0 = Instant::now();
        assert!(matches!(action_of(&all(&mut s, t0), "y"), QuoteAction::Place { .. }), "one side alone places");
        live(&mut s, "y", dec!(0.49));
        for secs in [0, 31, 3600] {
            assert_eq!(action_of(&all(&mut s, t0 + Duration::from_secs(secs)), "y"), QuoteAction::Hold);
        }
        assert!(s.pair_alone_since.is_empty() && s.pair_wait.is_empty());
        // Exactly 10¢ / 90¢ still allows one side (same rule as the dashboard).
        // The quoter re-pegs to the new best bid as usual; the rule adds nothing.
        s.books.insert("YES".into(), book(dec!(0.89), dec!(0.91), dec!(100)));
        assert_eq!(
            action_of(&all(&mut s, t0 + Duration::from_secs(7200)), "y"),
            QuoteAction::Replace { order_id: "ord-y".into(), new_price: dec!(0.88) }
        );
        assert!(s.pair_alone_since.is_empty());
    }

    #[test]
    fn the_midpoint_moving_back_inside_clears_the_clock() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let t0 = Instant::now();
        all(&mut s, t0);
        s.books.insert("YES".into(), book(dec!(0.85), dec!(0.86), dec!(100)));
        all(&mut s, t0 + Duration::from_secs(10));
        assert!(s.pair_alone_since.is_empty());
        s.books.insert("YES".into(), book(dec!(0.95), dec!(0.96), dec!(100)));
        assert_eq!(action_of(&all(&mut s, t0 + PAIR_GRACE), "y"), QuoteAction::Hold, "clock restarted at the crossing");
    }

    #[test]
    fn the_midpoint_comes_from_the_other_side_when_this_book_is_one_sided() {
        let mut s = state(dec!(100), dec!(0));
        s.books.get_mut("YES").unwrap().best_ask = None;
        assert_eq!(action_of(&all(&mut s, Instant::now()), "y"), QuoteAction::Hold);
        assert_eq!(s.pair_wait.get("y"), Some(&PairWait::OtherSide));
        // No midpoint anywhere: the rule can't tell, so quoting is as before.
        s.books.get_mut("NO").unwrap().best_ask = None;
        assert!(matches!(action_of(&all(&mut s, Instant::now()), "y"), QuoteAction::Place { .. }));
    }

    #[test]
    fn own_cancels_deactivations_and_re_pegs_pass_through() {
        let now = Instant::now();
        // Paused with a live order: deactivates as before, alone or not.
        let mut s = state(dec!(100), dec!(0));
        s.configs[0].paused = true;
        live(&mut s, "y", dec!(0.94));
        assert!(matches!(action_of(&all(&mut s, now), "y"), QuoteAction::Deactivate { .. }));
        // Depth gone under a live order: its own cancel.
        let mut s = state(dec!(0), dec!(0));
        live(&mut s, "y", dec!(0.94));
        assert!(matches!(
            action_of(&all(&mut s, now), "y"),
            QuoteAction::Cancel { reason: CancelReason::DepthDropped, .. }
        ));
        // Both live, best bid moved into our order: a normal re-peg.
        let mut s = state(dec!(100), dec!(100));
        live(&mut s, "y", dec!(0.95));
        live(&mut s, "n", dec!(0.03));
        assert!(matches!(action_of(&all(&mut s, now), "y"), QuoteAction::Replace { .. }));
    }

    #[test]
    fn only_the_same_market_and_the_other_outcome_pair_up() {
        let now = Instant::now();
        let mut s = state(dec!(100), dec!(100));
        // Another market's No is live; ours is not farmed.
        s.configs[1].condition_id = "0xOther".into();
        live(&mut s, "n", dec!(0.03));
        assert_eq!(action_of(&all(&mut s, now), "y"), QuoteAction::Hold);
        assert_eq!(s.pair_wait.get("y"), Some(&PairWait::NoOtherSide));
        // A second Yes leg on the same market is not "the other side".
        let mut s = state(dec!(100), dec!(100));
        s.configs[1] = leg("y2", "YES", CID);
        live(&mut s, "y2", dec!(0.94));
        assert_eq!(action_of(&all(&mut s, now), "y"), QuoteAction::Hold);
        // Market ids compare without case; an empty id never pairs.
        let mut s = state(dec!(100), dec!(100));
        s.configs[1].condition_id = CID.to_lowercase();
        assert!(matches!(action_of(&all(&mut s, now), "y"), QuoteAction::Place { .. }));
        let mut s = state(dec!(100), dec!(100));
        s.configs[0].condition_id.clear();
        s.configs[1].condition_id.clear();
        assert_eq!(action_of(&all(&mut s, now), "y"), QuoteAction::Hold);
    }

    #[test]
    fn heartbeat_failure_blocks_pairing_like_it_blocks_placing() {
        let mut s = state(dec!(100), dec!(100));
        s.heartbeat_paused = true;
        let d = all(&mut s, Instant::now());
        assert_eq!(action_of(&d, "y"), QuoteAction::Hold);
        assert_eq!(action_of(&d, "n"), QuoteAction::Hold);
        assert!(s.pair_wait.is_empty(), "the heartbeat holds them, not the two-sided rule");
    }

    #[test]
    fn selection_limits_what_is_evaluated_and_bookkeeping_follows_removals() {
        let mut s = state(dec!(100), dec!(0));
        live(&mut s, "y", dec!(0.94));
        let d = decide(&mut s, |c| c.id == "n", Instant::now());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, "n");
        all(&mut s, Instant::now());
        assert!(s.pair_alone_since.contains_key("y"));
        s.configs.retain(|c| c.id != "y");
        s.pair_pulls.insert("y".into(), 2);
        s.pair_wait.insert("y".into(), PairWait::OtherSide);
        all(&mut s, Instant::now());
        assert!(s.pair_alone_since.is_empty() && s.pair_pulls.is_empty() && !s.pair_wait.contains_key("y"));
    }

    #[test]
    fn bookkeeping_stays_bounded_over_many_cycles() {
        let mut s = state(dec!(100), dec!(0));
        let t0 = Instant::now();
        for i in 0..20_000u64 {
            let now = t0 + Duration::from_secs(i);
            // Churn: legs come and go, orders go up and down.
            let k = i % 7;
            s.configs.push(leg(&format!("x{i}"), &format!("T{k}"), &format!("0x{k}")));
            if s.configs.len() > 12 {
                s.configs.remove(2);
            }
            if i % 3 == 0 {
                live(&mut s, "y", dec!(0.94));
            } else if i % 3 == 1 {
                s.order_status.insert("y".into(), OrderStatus::Idle);
            }
            all(&mut s, now);
            assert!(s.pair_alone_since.len() <= s.configs.len());
            assert!(s.pair_pulls.len() <= s.configs.len());
            assert!(s.pair_wait.len() <= s.configs.len());
        }
    }

    /// A randomized run of the engine loop: books drift across the 10¢/90¢
    /// boundary, depth comes and goes, placements succeed or fail (sometimes
    /// persistently, like an empty balance), legs get paused and resumed, and
    /// every action is carried out with a second of latency. Checked each step:
    /// * outside 10–90¢ no order stays up without a live order on the other
    ///   side for longer than the grace period plus the cancel's latency;
    /// * inside 10–90¢ every decision is exactly the plain quoter's;
    /// * pulls are rate-limited by the back-off (no place-and-cancel loop).
    #[test]
    fn randomized_engine_loop_keeps_the_two_sided_guarantees() {
        let pulls: usize = (1..=24u64).map(|seed| run_sim(seed, 4 * 3600)).sum();
        assert!(pulls > 50, "the runs must exercise the pull path ({pulls} pulls)");
    }

    /// Returns how many times the rule pulled an order.
    fn run_sim(seed: u64, steps: u64) -> usize {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let mut s = state(dec!(100), dec!(100));
        let t0 = Instant::now();
        // In-flight work finishing next step: (leg, outcome).
        enum Done {
            Placed(String, Decimal),
            Failed(String),
            Cancelled(String),
            Replaced(String, Decimal),
        }
        let mut pending: Vec<Done> = Vec::new();
        let mut yes_mid = dec!(0.95);
        let mut fail_no_until = 0u64; // the No side can't place (e.g. no balance)
        let (mut thin_y, mut thin_n) = (false, false);
        let mut order_since: HashMap<String, u64> = HashMap::new(); // own tracker
        let mut pulls_at: Vec<u64> = Vec::new();
        let mut pulls_seen = 0usize;

        for t in 0..steps {
            let now = t0 + Duration::from_secs(t);
            // 1. Finish last step's work.
            for d in pending.drain(..) {
                match d {
                    Done::Placed(id, p) => {
                        if matches!(s.order_status.get(&id), Some(OrderStatus::Placing { .. })) {
                            s.order_status
                                .insert(id.clone(), OrderStatus::Live { order_id: format!("o{id}{t}"), price: p });
                        }
                    }
                    Done::Failed(id) => {
                        s.order_status.insert(id.clone(), OrderStatus::Idle);
                        if next(2) == 0 {
                            s.place_retry_at.insert(id, now + Duration::from_secs(2 + next(30)));
                        }
                    }
                    Done::Cancelled(id) => {
                        s.order_status.insert(id, OrderStatus::Idle);
                    }
                    Done::Replaced(id, p) => {
                        s.order_status
                            .insert(id.clone(), OrderStatus::Live { order_id: format!("o{id}{t}"), price: p });
                    }
                }
            }
            // 2. The market moves.
            if next(20) == 0 {
                let step = Decimal::from(next(5) as i64 - 2) / dec!(100);
                yes_mid = (yes_mid + step).clamp(dec!(0.03), dec!(0.97));
            }
            if next(400) == 0 {
                yes_mid = if next(2) == 0 { dec!(0.50) } else { dec!(0.95) };
            }
            // Depth on each side changes every few minutes, with brief flickers.
            if next(180) == 0 {
                thin_y = !thin_y;
            }
            if next(120) == 0 {
                thin_n = !thin_n;
            }
            let flicker = next(50) == 0;
            let depth = |thin: bool| if thin { dec!(0) } else { dec!(100) };
            let (dy, dn) = (depth(thin_y || flicker), depth(thin_n));
            let bid = yes_mid - dec!(0.005);
            s.books.insert("YES".into(), book(bid, bid + dec!(0.01), dy));
            let nbid = Decimal::ONE - (bid + dec!(0.01));
            s.books.insert("NO".into(), book(nbid, nbid + dec!(0.01), dn));
            if next(1500) == 0 {
                fail_no_until = t + 600 + next(1800);
            }
            if next(2000) == 0 {
                let i = next(2) as usize;
                s.configs[i].paused = !s.configs[i].paused;
            }

            // 3. Decide.
            let raw: Vec<(String, QuoteAction)> = s
                .configs
                .iter()
                .map(|c| {
                    let st = s.order_status.get(&c.id).cloned().unwrap_or_default();
                    (c.id.clone(), quoter::evaluate(c, &s.books[&c.token_id], &st))
                })
                .collect();
            let gated = !single_sided_allowed(yes_mid);
            let decisions = all(&mut s, now);
            let well_inside = yes_mid >= CLEARLY_INSIDE.0 && yes_mid <= CLEARLY_INSIDE.1;
            for (id, a) in &raw {
                let got = action_of(&decisions, id);
                if well_inside {
                    assert_eq!(&got, a, "seed {seed} t {t}: well inside 10–90¢ the quoter decides");
                } else if !gated {
                    // At the edges only a back-off can hold a placement.
                    assert!(
                        &got == a || (matches!(a, QuoteAction::Place { .. }) && got == QuoteAction::Hold),
                        "seed {seed} t {t}"
                    );
                }
            }

            // 4. Execute, as the engine does.
            for (id, action, _, _, _) in decisions {
                match action {
                    QuoteAction::Hold => {}
                    QuoteAction::Place { price } => {
                        if placer::begin_place(&mut s, &id, price, now).is_some() {
                            let fails = (id == "n" && t < fail_no_until) || next(10) == 0;
                            pending.push(if fails { Done::Failed(id) } else { Done::Placed(id, price) });
                        }
                    }
                    QuoteAction::Cancel { order_id, reason } => {
                        if reason != CancelReason::DepthDropped {
                            pulls_at.push(t);
                            pulls_seen += 1;
                        }
                        s.order_status
                            .insert(id.clone(), OrderStatus::Cancelling { order_id, since: std::time::Instant::now() });
                        pending.push(Done::Cancelled(id));
                    }
                    QuoteAction::Replace { order_id, new_price } => {
                        s.order_status
                            .insert(id.clone(), OrderStatus::Cancelling { order_id, since: std::time::Instant::now() });
                        pending.push(Done::Replaced(id, new_price));
                    }
                    QuoteAction::Deactivate { order_id, .. } => match order_id {
                        Some(oid) => {
                            s.order_status.insert(
                                id.clone(),
                                OrderStatus::Cancelling { order_id: oid, since: std::time::Instant::now() },
                            );
                            pending.push(Done::Cancelled(id));
                        }
                        None => {
                            s.order_status.insert(id, OrderStatus::Idle);
                        }
                    },
                }
            }

            // 5. Invariant: outside 10–90¢, an order (live or in flight) without a
            // live order on the other side is gone within grace + latency.
            for (me, other) in [("y", "n"), ("n", "y")] {
                let has_order = !matches!(s.order_status.get(me), None | Some(OrderStatus::Idle));
                let other_live = matches!(s.order_status.get(other), Some(OrderStatus::Live { .. }));
                if gated && has_order && !other_live {
                    let since = *order_since.entry(me.to_string()).or_insert(t);
                    assert!(
                        t - since <= PAIR_GRACE.as_secs() + 3,
                        "seed {seed} t {t}: {me} alone since {since} ({:?} / {:?})",
                        s.order_status.get(me),
                        s.order_status.get(other)
                    );
                } else {
                    order_since.remove(me);
                }
            }
        }
        // 6. Back-off: no window of 10 minutes has more than a handful of pulls
        // (30 + 60 + 120 + 240 s of back-off fit in 10 min, plus the grace).
        for (i, &t) in pulls_at.iter().enumerate() {
            let in_window = pulls_at[i..].iter().take_while(|&&u| u < t + 600).count();
            assert!(in_window <= 6, "seed {seed}: {in_window} pulls within 10 min from t={t}");
        }
        pulls_seen
    }
}
