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
use crate::types::OrderStatus;

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
            (Some(size), Some(attempt)) => Some((attempt, size)),
            _ => None,
        }
    };
    let Some((attempt, order_size)) = claimed else { return };
    tokio::spawn(placer::run_place(
        Arc::clone(api),
        Arc::clone(state),
        Arc::clone(alerter),
        config_id,
        attempt,
        token_id,
        order_size,
        price,
        tick_size,
    ));
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

/// Re-peg: check the new price fits the order library's tick first (with a
/// stale tick, cancelling would leave the leg with no order), cancel the old
/// order, and once it's confirmed gone hand the leg back to the decision path:
/// Idle, marked as a re-peg (the "Order replaced" alert), and a nudge so the
/// engine decides — and places — straight away.
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
    let prev = {
        let mut s = state.write().await;
        if s.heartbeat_paused {
            return; // Polymarket unreachable: don't take a working order down
        }
        let prev = s.order_status.get(&config_id).cloned();
        if !placer::begin_cancel(&mut s, &config_id, &order_id) {
            return;
        }
        prev
    };
    let (api, state, alerter, nudge) = (Arc::clone(api), Arc::clone(state), Arc::clone(alerter), Arc::clone(nudge));
    tokio::spawn(async move {
        let tick = placer::check_tick(&*api, &state, &alerter, &token_id, tick_size, new_price).await;
        if tick != placer::TickCheck::Ok {
            if let placer::TickCheck::Error(e) = &tick {
                warn!("Replace skipped: {e}");
            }
            // Keep the working order: put the leg back as it was.
            let mut s = state.write().await;
            if matches!(s.order_status.get(&config_id),
                Some(OrderStatus::Cancelling { order_id: oid, .. }) if *oid == order_id)
            {
                let back = match prev {
                    Some(live @ OrderStatus::Live { .. }) => live,
                    _ => OrderStatus::Idle,
                };
                s.order_status.insert(config_id, back);
            }
            return;
        }
        match api.cancel_verified(&order_id).await {
            Ok(true) => {
                let freed = {
                    let mut s = state.write().await;
                    let freed = placer::free_if_cancelling(&mut s, &config_id, &order_id);
                    if freed {
                        s.repegging.insert(config_id);
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
    });
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
}
