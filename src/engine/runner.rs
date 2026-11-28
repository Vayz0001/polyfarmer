//! Carry out the engine's decisions: the one place where a [`Decision`] turns
//! into calls to Polymarket. Used by both triggers — book updates
//! (`ws_manager`) and the 30 s timer (`app`) — so they can't drift apart, and
//! generic over [`OrderApi`] so it runs against the test fake too.
//!
//! Every state change here is a compare-and-set: a placement only claims an
//! Idle leg (`placer::begin_place`), a cancel only claims the order the leg
//! still holds (`placer::begin_cancel`). A decision made a moment ago that no
//! longer matches the leg does nothing.
//!
//! A re-peg (Replace) cancels the old order and, once Polymarket confirms it
//! gone, hands the leg back to the normal decision path (Idle + a nudge), so the
//! new order goes through every check a first placement does: depth ahead, the
//! two-sided rule, tick handling, retries and "not enough balance".

use std::sync::Arc;

use tokio::sync::{Notify, RwLock};
use tracing::{error, warn};

use crate::engine::alerts::Alerter;
use crate::engine::pairing::Decision;
use crate::engine::placer::{self, OrderApi};
use crate::engine::quoter::{DeactivateReason, QuoteAction};
use crate::engine::ws_manager::AppState;

/// Run one batch of decisions. Placements and cancels are spawned; the
/// function returns once each decision has claimed (or skipped) its leg.
pub async fn execute<A: OrderApi>(
    decisions: Vec<Decision>,
    api: &Arc<A>,
    state: &Arc<RwLock<AppState>>,
    alerter: &Arc<Alerter>,
    nudge: &Arc<Notify>,
) {
    for d in decisions {
        let Decision { config_id, action, token_id, tick_size } = d;
        match action {
            QuoteAction::Hold => {}
            QuoteAction::Place { price } => place(api, state, alerter, config_id, token_id, price, tick_size).await,
            QuoteAction::Cancel { order_id, reason } => {
                cancel(api, state, alerter, config_id, order_id, reason.describe()).await;
            }
            QuoteAction::Replace { order_id, new_price } => {
                replace(api, state, alerter, nudge, config_id, order_id, token_id, new_price, tick_size).await;
            }
            QuoteAction::Deactivate { order_id, reason } => {
                deactivate(api, state, alerter, config_id, order_id, reason).await;
            }
        }
    }
}

/// Claim the slot (Idle, heartbeat healthy, no retry delay) so concurrent
/// events never place twice, read the leg's size at that moment, and place.
async fn place<A: OrderApi>(
    api: &Arc<A>,
    state: &Arc<RwLock<AppState>>,
    alerter: &Arc<Alerter>,
    config_id: String,
    token_id: String,
    price: rust_decimal::Decimal,
    tick_size: rust_decimal::Decimal,
) {
    let claimed = {
        let mut s = state.write().await;
        let size = s.configs.iter().find(|c| c.id == config_id).map(|c| c.order_size);
        match (size, placer::begin_place(&mut s, &config_id, price, tokio::time::Instant::now())) {
            (Some(size), Some(attempt)) => Some((attempt, size, placer::InFlight::start(&s))),
            _ => None,
        }
    };
    let Some((attempt, order_size, in_flight)) = claimed else { return };
    let (api, state, alerter) = (Arc::clone(api), Arc::clone(state), Arc::clone(alerter));
    tokio::spawn(async move {
        // Counted until the call ends, even if the leg is released meanwhile.
        let _in_flight = in_flight;
        placer::run_place(api, state, alerter, config_id, attempt, token_id, order_size, price, tick_size).await
    });
}

/// The leg's label and outcome, for alerts (`None` once it was removed).
fn names(s: &AppState, config_id: &str) -> Option<(String, String, rust_decimal::Decimal)> {
    s.configs.iter().find(|c| c.id == config_id).map(|c| (c.label.clone(), c.token_label.clone(), c.order_size))
}

/// Cancel the leg's order (only if it still holds it) and free the leg once
/// Polymarket confirms it gone. Unconfirmed cancels stay `Cancelling`; the
/// timer retries them (`placer::confirm_stuck_cancel`).
async fn cancel<A: OrderApi>(
    api: &Arc<A>,
    state: &Arc<RwLock<AppState>>,
    alerter: &Arc<Alerter>,
    config_id: String,
    order_id: String,
    why: &'static str,
) {
    let leg = {
        let mut s = state.write().await;
        if !placer::begin_cancel(&mut s, &config_id, &order_id) {
            return;
        }
        names(&s, &config_id)
    };
    let (api, state, alerter) = (Arc::clone(api), Arc::clone(state), Arc::clone(alerter));
    tokio::spawn(async move {
        match api.cancel_verified(&order_id).await {
            Ok(true) => {
                if let Some((label, token_label, size)) = leg {
                    alerter.info(format!("Order cancelled · {label}\n${size} of {token_label} — {why}"));
                }
                placer::free_if_cancelling(&mut *state.write().await, &config_id, &order_id);
            }
            Ok(false) => {
                warn!("Order {order_id} still on the book after a verified cancel — the timer retries it");
                alerter.warn(format!("Order {order_id} cancel unconfirmed (still live) — will retry via 30s timer"));
            }
            Err(e) => error!("Cancel order {order_id} failed (the timer retries it): {e}"),
        }
    });
}

/// After a tick-size read fails during a re-peg, wait this long before the leg
/// tries to re-peg again (it keeps its working order meanwhile).
const REPEG_TICK_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

/// Re-peg. First check the new price fits the order library's tick while the
/// old order still stands (with a stale tick, cancelling would leave the leg
/// with no order) — nothing is claimed yet, so a failed check has nothing to
/// undo and can't disturb a cancel someone else started. Then claim the order
/// (only if the leg still holds it), cancel it, and once it's confirmed gone
/// hand the leg back to the decision path: Idle, marked as a re-peg (the
/// "Order replaced" alert), and a nudge so the engine decides — and places —
/// straight away. One re-peg per leg at a time; a failed tick read waits
/// [`REPEG_TICK_RETRY`] before the next.
#[allow(clippy::too_many_arguments)]
async fn replace<A: OrderApi>(
    api: &Arc<A>,
    state: &Arc<RwLock<AppState>>,
    alerter: &Arc<Alerter>,
    nudge: &Arc<Notify>,
    config_id: String,
    order_id: String,
    token_id: String,
    new_price: rust_decimal::Decimal,
    tick_size: rust_decimal::Decimal,
) {
    {
        let mut s = state.write().await;
        if s.heartbeat_paused {
            return; // Polymarket unreachable: don't take a working order down
        }
        if s.repeg_retry_at.get(&config_id).is_some_and(|t| tokio::time::Instant::now() < *t) {
            return; // the last tick read failed moments ago
        }
        if !s.repegging_now.insert(config_id.clone()) {
            return; // already re-pegging this leg
        }
    }
    let (api, state, alerter, nudge) = (Arc::clone(api), Arc::clone(state), Arc::clone(alerter), Arc::clone(nudge));
    tokio::spawn(async move {
        repeg(&*api, &state, &alerter, &nudge, &config_id, &order_id, &token_id, new_price, tick_size).await;
        state.write().await.repegging_now.remove(&config_id);
    });
}

#[allow(clippy::too_many_arguments)]
async fn repeg<A: OrderApi>(
    api: &A,
    state: &Arc<RwLock<AppState>>,
    alerter: &Alerter,
    nudge: &Notify,
    config_id: &str,
    order_id: &str,
    token_id: &str,
    new_price: rust_decimal::Decimal,
    tick_size: rust_decimal::Decimal,
) {
    match placer::check_tick(api, state, alerter, token_id, tick_size, new_price).await {
        placer::TickCheck::Ok => {}
        // The tick changed and the config adopted it: the next decision re-quotes.
        placer::TickCheck::Requote => return,
        placer::TickCheck::Error(e) => {
            warn!("Replace skipped (keeping the current order): {e}");
            state
                .write()
                .await
                .repeg_retry_at
                .insert(config_id.to_string(), tokio::time::Instant::now() + REPEG_TICK_RETRY);
            return;
        }
    }
    // Claim the order now, only if the leg still holds it.
    if !placer::begin_cancel(&mut *state.write().await, config_id, order_id) {
        return;
    }
    match api.cancel_verified(order_id).await {
        Ok(true) => {
            let freed = {
                let mut s = state.write().await;
                let freed = placer::free_if_cancelling(&mut s, config_id, order_id);
                if freed {
                    s.repegging.insert(config_id.to_string());
                    s.repeg_retry_at.remove(config_id);
                }
                freed
            };
            if freed {
                nudge.notify_one();
            }
        }
        // Still on the book, or unknown: stay Cancelling; the timer retries
        // until Polymarket confirms, and nothing is placed beside it.
        Ok(false) => warn!("Replace: {order_id} still on the book after a verified cancel — the timer retries it"),
        Err(e) => error!("Replace: cancel of {order_id} failed (the timer retries it): {e}"),
    }
}

/// Paused, expired or auto-paused: alert once, then cancel the order the leg
/// holds (or settle a placement in flight). An auto-pause is saved first.
async fn deactivate<A: OrderApi>(
    api: &Arc<A>,
    state: &Arc<RwLock<AppState>>,
    alerter: &Arc<Alerter>,
    config_id: String,
    order_id: Option<String>,
    reason: DeactivateReason,
) {
    let label = {
        let s = state.read().await;
        s.configs.iter().find(|c| c.id == config_id).map(|c| c.label.clone()).unwrap_or_else(|| config_id.clone())
    };
    match reason {
        DeactivateReason::Volatility => {
            // Auto-pause so the bot doesn't re-place while the market is swinging.
            if let Some((_, Err(e))) = placer::auto_pause(state, &config_id).await {
                error!("Failed to save markets after volatility pause of '{label}': {e}");
                alerter.error(format!(
                    "Auto-pause of '{label}' could not be saved: {e}\nIt is paused now but would resume after a restart."
                ));
            }
            alerter.warn(format!(
                "Auto-paused · {label}\nBest bid moved past your auto-pause limit — resume from Markets to restart \
                 from the current price"
            ));
        }
        DeactivateReason::Paused => alerter.info(format!("Paused · {label}\nOrder cancelled — resume from Markets")),
        DeactivateReason::Expired => {
            alerter.info(format!("Expired · {label}\nOrder cancelled — edit the leg to extend it"));
        }
    }

    // Mark Cancelling now, so later book events hold instead of alerting again.
    // Claim the decided order if the leg still holds it; otherwise settle what
    // it holds now: an in-flight placement is freed (it cancels its own order),
    // one that landed meanwhile is cancelled — never left untracked.
    let target = {
        let mut s = state.write().await;
        match &order_id {
            Some(oid) if placer::begin_cancel(&mut s, &config_id, oid) => Some(oid.clone()),
            _ => placer::settle_unplaced(&mut s, &config_id),
        }
    };
    if let Some(oid) = target {
        let (api, state) = (Arc::clone(api), Arc::clone(state));
        tokio::spawn(async move { placer::cancel_deactivated(&*api, &state, &config_id, &oid).await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::quoter::CancelReason;
    use crate::engine::test_support::*;
    use crate::types::MarketConfig;
    use crate::types::OrderStatus;
    use rust_decimal_macros::dec;
    use std::time::Duration;

    fn d(id: &str, action: QuoteAction) -> Decision {
        Decision { config_id: id.into(), action, token_id: TOKEN.into(), tick_size: dec!(0.01) }
    }
    fn live(o: &str, p: rust_decimal::Decimal) -> OrderStatus {
        OrderStatus::Live { order_id: o.into(), price: p }
    }
    /// Let the spawned placement/cancel tasks finish.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    async fn run(e: &Env, api: &Arc<Fake>, nudge: &Arc<Notify>, decisions: Vec<Decision>) {
        execute(decisions, api, &e.state, &e.alerter, nudge).await;
        settle().await;
    }
    fn setup(legs: Vec<MarketConfig>) -> (Env, Arc<Fake>, Arc<Notify>) {
        (env(legs), Fake::new(dec!(0.01), dec!(0.01)), Arc::new(Notify::new()))
    }

    #[tokio::test]
    async fn place_claims_an_idle_leg_and_sizes_at_claim_time() {
        let (e, api, n) = setup(vec![leg("a", dec!(0.01))]);
        e.state.write().await.configs[0].order_size = dec!(30); // edited after the decision
        run(&e, &api, &n, vec![d("a", QuoteAction::Place { price: dec!(0.20) })]).await;
        assert_eq!(*api.placed.lock().unwrap(), vec![dec!(0.20)]);
        assert_eq!(*api.placed_shares.lock().unwrap(), vec![dec!(150)], "$30 / 20¢, read when claimed");
        assert!(matches!(e.state.read().await.order_status.get("a"), Some(OrderStatus::Live { .. })));
        assert!(alerts(&e).contains("Order placed · leg a"));

        // A second Place for a leg that is no longer Idle does nothing.
        run(&e, &api, &n, vec![d("a", QuoteAction::Place { price: dec!(0.21) })]).await;
        assert_eq!(api.placed.lock().unwrap().len(), 1);
    }

    /// Each placement the runner starts is counted while its call is out (for
    /// shutdown) and released when the call ends, whatever the outcome.
    #[tokio::test]
    async fn placement_calls_are_counted_until_they_end_whatever_the_outcome() {
        let (e, api, n) = setup(vec![leg("a", dec!(0.01)), leg("b", dec!(0.01)), leg("c", dec!(0.01))]);
        let answer = Arc::new(Notify::new());
        *api.place_gate.lock().unwrap() = Some(Arc::clone(&answer));
        api.script(Ok(()));
        api.script(Err("invalid order size"));
        api.script(Err("Internal: error sending request"));
        let place = |id: &str| d(id, QuoteAction::Place { price: dec!(0.20) });
        run(&e, &api, &n, vec![place("a"), place("b"), place("c")]).await;
        assert_eq!(e.state.read().await.placements_in_flight.load(std::sync::atomic::Ordering::SeqCst), 3);
        answer.notify_waiters(); // Polymarket answers all three
        settle().await;
        let s = e.state.read().await;
        assert!(matches!(s.order_status.get("a"), Some(OrderStatus::Live { .. })), "placed");
        assert_eq!(s.place_failures.get("b"), Some(&1), "rejected");
        assert_eq!(s.place_outage.get("c"), Some(&1), "outage");
        assert_eq!(s.placements_in_flight.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancel_frees_the_leg_and_a_stale_cancel_touches_nothing() {
        let (e, api, n) = setup(vec![leg("a", dec!(0.01))]);
        e.state.write().await.order_status.insert("a".into(), live("o1", dec!(0.2)));
        let cancel = |o: &str| QuoteAction::Cancel { order_id: o.into(), reason: CancelReason::DepthDropped };
        run(&e, &api, &n, vec![d("a", cancel("o1"))]).await;
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));
        assert!(alerts(&e).contains("Order cancelled · leg a"));

        // The leg now holds a newer order: a decision about the old one is dropped.
        e.state.write().await.order_status.insert("a".into(), live("o2", dec!(0.2)));
        run(&e, &api, &n, vec![d("a", cancel("o1"))]).await;
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&live("o2", dec!(0.2))));
        assert_eq!(*api.cancelled.lock().unwrap(), vec!["o1".to_string()], "no call for the stale decision");
    }

    /// A re-peg takes the old order down, then hands the leg back to the normal
    /// placement path (Idle + nudge); that placement is announced as a replace.
    #[tokio::test]
    async fn replace_cancels_then_hands_back_to_normal_placement() {
        let (e, api, n) = setup(vec![leg("a", dec!(0.01))]);
        e.state.write().await.order_status.insert("a".into(), live("o1", dec!(0.20)));
        run(&e, &api, &n, vec![d("a", QuoteAction::Replace { order_id: "o1".into(), new_price: dec!(0.18) })]).await;
        {
            let s = e.state.read().await;
            assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle));
            assert!(s.repegging.contains("a"));
        }
        assert!(api.placed.lock().unwrap().is_empty(), "nothing placed by the replace itself");
        assert!(tokio::time::timeout(Duration::from_millis(50), n.notified()).await.is_ok(), "engine nudged");

        // The engine's next decision places it, through every normal check.
        run(&e, &api, &n, vec![d("a", QuoteAction::Place { price: dec!(0.18) })]).await;
        assert_eq!(*api.placed.lock().unwrap(), vec![dec!(0.18)]);
        assert!(alerts(&e).contains("Order replaced · leg a"));
        assert!(!e.state.read().await.repegging.contains("a"));
    }

    #[tokio::test]
    async fn replace_keeps_cancelling_when_the_cancel_is_unconfirmed_or_the_feed_is_down() {
        let (e, api, n) = setup(vec![leg("a", dec!(0.01))]);
        e.state.write().await.order_status.insert("a".into(), live("o1", dec!(0.20)));
        api.verify.lock().unwrap().push_back(Ok(false)); // still on the book
        run(&e, &api, &n, vec![d("a", QuoteAction::Replace { order_id: "o1".into(), new_price: dec!(0.18) })]).await;
        {
            let s = e.state.read().await;
            assert!(
                matches!(s.order_status.get("a"), Some(OrderStatus::Cancelling { order_id, .. }) if order_id == "o1")
            );
            assert!(s.repegging.is_empty());
        }
        assert!(tokio::time::timeout(Duration::from_millis(30), n.notified()).await.is_err(), "no nudge");

        // Heartbeat lost: a working order is not taken down.
        let (e, api, n) = setup(vec![leg("a", dec!(0.01))]);
        {
            let mut s = e.state.write().await;
            s.order_status.insert("a".into(), live("o1", dec!(0.20)));
            s.heartbeat_paused = true;
        }
        run(&e, &api, &n, vec![d("a", QuoteAction::Replace { order_id: "o1".into(), new_price: dec!(0.18) })]).await;
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&live("o1", dec!(0.20))));
        assert!(api.cancelled.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deactivate_cancels_whatever_the_leg_holds_now() {
        // Paused with a live order: cancelled, freed once confirmed, one alert.
        let (e, api, n) = setup(vec![leg("a", dec!(0.01))]);
        e.state.write().await.order_status.insert("a".into(), live("o1", dec!(0.2)));
        let off = |o: Option<&str>| QuoteAction::Deactivate {
            order_id: o.map(str::to_string),
            reason: DeactivateReason::Paused,
        };
        run(&e, &api, &n, vec![d("a", off(Some("o1")))]).await;
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));
        assert_eq!(*api.cancelled.lock().unwrap(), vec!["o1".to_string()]);
        assert_eq!(alerts(&e).matches("Paused · leg a").count(), 1);

        // Decided with no order, but one landed meanwhile: it is cancelled, not dropped.
        e.state.write().await.order_status.insert("a".into(), live("landed", dec!(0.2)));
        run(&e, &api, &n, vec![d("a", off(None))]).await;
        assert!(api.cancelled.lock().unwrap().contains(&"landed".to_string()));
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));
    }

    #[tokio::test]
    async fn an_auto_pause_is_saved_and_alerted() {
        let (e, api, n) = setup(vec![leg("a", dec!(0.01))]);
        e.state.write().await.order_status.insert("a".into(), live("o1", dec!(0.2)));
        let trip = QuoteAction::Deactivate { order_id: Some("o1".into()), reason: DeactivateReason::Volatility };
        run(&e, &api, &n, vec![d("a", trip)]).await;
        assert!(e.state.read().await.configs[0].paused);
        let saved: Vec<MarketConfig> =
            serde_json::from_str(&std::fs::read_to_string(e.dir.join("markets.json")).unwrap()).unwrap();
        assert!(saved[0].paused, "saved");
        assert!(alerts(&e).contains("Auto-paused · leg a"));
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));
    }

    /// End to end through the real decision step: a re-peg's new order goes
    /// through the depth check like any placement. Before, the replace placed at
    /// the new price straight after the cancel, even if the depth ahead had gone.
    #[tokio::test]
    async fn a_re_peg_only_re_places_when_depth_ahead_still_holds() {
        use crate::engine::orderbook::TokenBook;
        let mut a = leg("a", dec!(0.01));
        a.distance = dec!(0.02);
        a.min_depth_between = dec!(400);
        let (e, api, n) = setup(vec![a]);
        let book = |others_usd: rust_decimal::Decimal| {
            let mut b = TokenBook { best_bid: Some(dec!(0.30)), best_ask: Some(dec!(0.31)), ..Default::default() };
            b.bids.insert(dec!(0.30), others_usd / dec!(0.30));
            b
        };
        {
            let mut s = e.state.write().await;
            s.books.insert(TOKEN.into(), book(dec!(500)));
            s.order_status.insert("a".into(), live("o1", dec!(0.25))); // 5¢ below: too far, re-peg to 28¢
        }
        let decide = |s: &mut AppState| crate::engine::pairing::decide(s, |_| true, tokio::time::Instant::now());
        let first = decide(&mut *e.state.write().await);
        assert!(matches!(first[0].action, QuoteAction::Replace { new_price, .. } if new_price == dec!(0.28)));
        run(&e, &api, &n, first).await;
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));

        // The book thins out before the engine re-places: only $100 ahead now.
        e.state.write().await.books.insert(TOKEN.into(), book(dec!(100)));
        let second = decide(&mut *e.state.write().await);
        assert_eq!(second[0].action, QuoteAction::Hold, "waits for depth instead of placing unprotected");
        run(&e, &api, &n, second).await;
        assert!(api.placed.lock().unwrap().is_empty());

        // Depth comes back: it re-places, announced as a replace.
        e.state.write().await.books.insert(TOKEN.into(), book(dec!(500)));
        let third = decide(&mut *e.state.write().await);
        run(&e, &api, &n, third).await;
        assert_eq!(*api.placed.lock().unwrap(), vec![dec!(0.28)]);
        assert!(alerts(&e).contains("Order replaced · leg a"));
    }

    /// Two legs on one outcome. Leg a's order is being cancelled, and
    /// Polymarket doesn't confirm it gone yet: it still rests in the book. It is
    /// ours, so it is no protection for leg b. Before, a leg being cancelled
    /// dropped out of our own orders, and b counted a's $500 as other people's
    /// depth ahead and placed with only $100 of others in front.
    #[tokio::test]
    async fn an_order_being_cancelled_is_never_depth_ahead_of_another_leg() {
        use crate::engine::orderbook::TokenBook;
        let mut a = leg("a", dec!(0.01));
        (a.distance, a.min_depth_between, a.order_size) = (dec!(0.01), dec!(50), dec!(500));
        let mut b = leg("b", dec!(0.01));
        (b.distance, b.min_depth_between) = (dec!(0.02), dec!(400));
        let (e, api, n) = setup(vec![a, b]);
        // Best bid 30¢ with $100 of other people's bids.
        let mut book = TokenBook { best_bid: Some(dec!(0.30)), best_ask: Some(dec!(0.31)), ..Default::default() };
        book.bids.insert(dec!(0.30), dec!(100) / dec!(0.30));
        e.state.write().await.books.insert(TOKEN.into(), book);
        let decide = |s: &mut AppState| crate::engine::pairing::decide(s, |_| true, tokio::time::Instant::now());
        let action = |d: &[Decision], id: &str| d.iter().find(|x| x.config_id == id).map(|x| x.action.clone());

        // a places $500 at 29¢; b (28¢, needs $400 ahead) waits.
        let first = decide(&mut *e.state.write().await);
        assert_eq!(action(&first, "a"), Some(QuoteAction::Place { price: dec!(0.29) }));
        assert_eq!(action(&first, "b"), Some(QuoteAction::Hold));
        run(&e, &api, &n, first).await;
        assert!(matches!(e.state.read().await.order_status.get("a"), Some(OrderStatus::Live { .. })));
        // The feed shows a's order at 29¢. Live, it is ours: b still waits.
        e.state.write().await.books.get_mut(TOKEN).unwrap().bids.insert(dec!(0.29), dec!(500) / dec!(0.29));
        assert_eq!(action(&decide(&mut *e.state.write().await), "b"), Some(QuoteAction::Hold));

        // a is paused; Polymarket says its order is still on the book.
        api.verify.lock().unwrap().push_back(Ok(false));
        e.state.write().await.configs[0].paused = true;
        let pause = decide(&mut *e.state.write().await);
        run(&e, &api, &n, pause).await;
        assert!(matches!(e.state.read().await.order_status.get("a"), Some(OrderStatus::Cancelling { .. })));
        let d = decide(&mut *e.state.write().await);
        assert_eq!(action(&d, "b"), Some(QuoteAction::Hold), "a's order still rests: only $100 of others ahead");

        // a's size edited while its cancel is pending: the order in the book is still $500.
        e.state.write().await.configs[0].order_size = dec!(50);
        let d = decide(&mut *e.state.write().await);
        assert_eq!(action(&d, "b"), Some(QuoteAction::Hold), "counted at the size it was placed with");
        run(&e, &api, &n, d).await;
        assert_eq!(*api.placed.lock().unwrap(), vec![dec!(0.29)], "b never placed");
    }

    /// Polymarket confirmed a cancel, but the feed's book still shows the order
    /// (its update hasn't arrived yet). That order is ours and gone: no
    /// protection for the leg re-placing behind it, nor for another leg on the
    /// outcome. Before, it counted as other people's depth the moment the leg
    /// let go of it, and both legs placed with only $100 of others ahead.
    #[tokio::test]
    async fn a_cancelled_order_is_never_depth_ahead_until_the_feed_shows_it_gone() {
        use crate::engine::orderbook::TokenBook;
        let mut a = leg("a", dec!(0.01));
        (a.distance, a.min_depth_between, a.order_size) = (dec!(0.02), dec!(400), dec!(500));
        let mut b = leg("b", dec!(0.01));
        (b.distance, b.min_depth_between) = (dec!(0.03), dec!(550));
        let (e, api, n) = setup(vec![a, b]);
        let book = |bid: rust_decimal::Decimal, others_usd: rust_decimal::Decimal| {
            let mut bk = TokenBook { best_bid: Some(bid), best_ask: Some(bid + dec!(0.01)), ..Default::default() };
            bk.bids.insert(bid, others_usd / bid);
            bk
        };
        e.state.write().await.books.insert(TOKEN.into(), book(dec!(0.30), dec!(450)));
        let decide = |s: &mut AppState| crate::engine::pairing::decide(s, |_| true, tokio::time::Instant::now());
        let action = |d: &[Decision], id: &str| d.iter().find(|x| x.config_id == id).map(|x| x.action.clone());

        // a places $500 at 28¢ ($450 ahead); b (27¢, needs $550) waits.
        let first = decide(&mut *e.state.write().await);
        assert_eq!(action(&first, "a"), Some(QuoteAction::Place { price: dec!(0.28) }));
        assert_eq!(action(&first, "b"), Some(QuoteAction::Hold));
        run(&e, &api, &n, first).await;
        let shares = api.placed_shares.lock().unwrap()[0];
        // The best bid drops to 29¢ with $100; the feed shows a's order at 28¢.
        let mut moved = book(dec!(0.29), dec!(100));
        moved.bids.insert(dec!(0.28), shares);
        e.state.write().await.books.insert(TOKEN.into(), moved);
        // a is now too close to the best bid, with too little ahead to move: cancelled, confirmed.
        let second = decide(&mut *e.state.write().await);
        assert!(matches!(action(&second, "a"), Some(QuoteAction::Cancel { reason: CancelReason::DepthDropped, .. })));
        run(&e, &api, &n, second).await;
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));

        // The feed still shows a's $500 at 28¢: only $100 of others ahead of 27¢ and 26¢.
        let d = decide(&mut *e.state.write().await);
        assert_eq!(action(&d, "a"), Some(QuoteAction::Hold), "a's own cancelled order is not depth");
        assert_eq!(action(&d, "b"), Some(QuoteAction::Hold), "nor is it for another leg");
        run(&e, &api, &n, d).await;
        // The feed catches up: still only $100 ahead.
        e.state.write().await.books.get_mut(TOKEN).unwrap().bids.remove(&dec!(0.28));
        let d = decide(&mut *e.state.write().await);
        assert_eq!((action(&d, "a"), action(&d, "b")), (Some(QuoteAction::Hold), Some(QuoteAction::Hold)));
        assert_eq!(*api.placed.lock().unwrap(), vec![dec!(0.28)], "nothing placed behind the ghost");
    }

    /// The race behind it: a Place is decided, then the leg is paused on the
    /// dashboard before the runner gets to it. Nothing may reach Polymarket.
    #[tokio::test]
    async fn a_place_decided_before_a_pause_places_nothing() {
        let (e, api, n) = setup(vec![leg("a", dec!(0.01))]);
        let decided = vec![d("a", QuoteAction::Place { price: dec!(0.20) })];
        e.state.write().await.configs[0].paused = true; // the pause lands in between
        run(&e, &api, &n, decided).await;
        assert!(api.placed.lock().unwrap().is_empty());
        assert_eq!(e.state.read().await.order_status.get("a"), None);
    }

    fn replace_to(price: rust_decimal::Decimal) -> QuoteAction {
        QuoteAction::Replace { order_id: "o1".into(), new_price: price }
    }
    /// A decision for a leg on a 0.1¢ tick.
    fn d_fine(id: &str, action: QuoteAction) -> Decision {
        Decision { tick_size: dec!(0.001), ..d(id, action) }
    }

    /// A tick read that fails leaves the working order alone (nothing was
    /// claimed), and the leg waits before trying to re-peg again — before, it
    /// retried on every book update, re-reading the tick each time.
    #[tokio::test(start_paused = true)]
    async fn a_failed_tick_read_keeps_the_order_and_waits_before_retrying() {
        let (e, _, n) = setup(vec![leg("a", dec!(0.001))]);
        let api = Fake::new(dec!(0.01), dec!(0.001)); // the order library's tick is stale
        *api.refresh_fails.lock().unwrap() = true; // and can't be re-read right now
        e.state.write().await.order_status.insert("a".into(), live("o1", dec!(0.213)));
        run(&e, &api, &n, vec![d_fine("a", replace_to(dec!(0.211)))]).await;
        {
            let s = e.state.read().await;
            assert_eq!(s.order_status.get("a"), Some(&live("o1", dec!(0.213))), "never claimed");
            assert!(s.repeg_retry_at.contains_key("a") && s.repegging_now.is_empty());
        }
        assert!(api.cancelled.lock().unwrap().is_empty());
        // Within the wait: a new Replace is skipped outright (no tick read).
        *api.refresh_fails.lock().unwrap() = false;
        run(&e, &api, &n, vec![d_fine("a", replace_to(dec!(0.211)))]).await;
        assert_eq!(*api.refreshes.lock().unwrap(), 0, "no tick read while waiting");
        // After it: the re-peg goes ahead.
        tokio::time::sleep(REPEG_TICK_RETRY).await;
        run(&e, &api, &n, vec![d_fine("a", replace_to(dec!(0.211)))]).await;
        assert_eq!(*api.refreshes.lock().unwrap(), 1);
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle), "cancelled for the re-peg");
    }

    /// The bug: someone else (the two-sided rule, auto-pause, the dashboard)
    /// was already cancelling the order; a re-peg whose tick read then failed
    /// "restored" the leg to Idle, dropping that cancel — the order stayed up
    /// while the leg could place a second one.
    #[tokio::test]
    async fn a_failed_re_peg_never_drops_someone_elses_cancel() {
        let (e, _, n) = setup(vec![leg("a", dec!(0.001))]);
        let api = Fake::new(dec!(0.01), dec!(0.001));
        *api.refresh_fails.lock().unwrap() = true;
        let since = std::time::Instant::now();
        e.state.write().await.order_status.insert("a".into(), OrderStatus::Cancelling { order_id: "o1".into(), since });
        run(&e, &api, &n, vec![d_fine("a", replace_to(dec!(0.211)))]).await;
        assert_eq!(
            e.state.read().await.order_status.get("a"),
            Some(&OrderStatus::Cancelling { order_id: "o1".into(), since }),
            "the pending cancel is left to finish"
        );
    }

    /// Two book updates deciding a re-peg at once: one re-peg runs, one cancel.
    #[tokio::test]
    async fn one_re_peg_per_leg_at_a_time() {
        let (e, api, n) = setup(vec![leg("a", dec!(0.01))]);
        e.state.write().await.order_status.insert("a".into(), live("o1", dec!(0.20)));
        run(&e, &api, &n, vec![d("a", replace_to(dec!(0.18))), d("a", replace_to(dec!(0.18)))]).await;
        assert_eq!(*api.cancelled.lock().unwrap(), vec!["o1".to_string()]);
        assert!(e.state.read().await.repegging_now.is_empty(), "the slot is freed afterwards");
    }
}
