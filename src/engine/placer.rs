//! Order placement shared by the WebSocket path and the timer path.
//!
//! Three things live here so both callers behave the same:
//!
//! * **Tick size.** Polymarket can change a market's tick size while the bot
//!   runs (e.g. 1¢ → 0.1¢). The saved config and the SDK's per-process tick
//!   cache can then disagree, and the SDK refuses prices the market accepts.
//!   Before each order we compare the two; on a mismatch we read the real tick
//!   from Polymarket, adopt it, and either place or re-quote. A tick change is
//!   never counted as a placement failure.
//! * **Backoff.** A failure delays the next attempt (2 s, 4 s, … up to a
//!   minute) instead of retrying on the next book update, so a burst of book
//!   events cannot use up the failure budget in milliseconds. An outage
//!   (network, timeout, 5xx, 429) only backs off and is never counted.
//! * **Pause after rejections.** After `MAX_PLACE_FAILURES` real rejections in
//!   a row the leg is paused and saved, with an alert giving the reason. A leg
//!   is never deleted by the engine.
//!
//! It also holds the cancel and auto-pause steps both callers share: a leg is
//! freed (back to Idle, so it may quote again) only once Polymarket confirms
//! its old order is gone, and an auto-pause is saved under the state lock.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::engine::ws_manager::{AppState, PlacedOrder, MAX_PLACE_FAILURES};
use crate::storage::save_markets;
use crate::types::OrderStatus;

/// First retry delay after a failed placement; doubles per failure.
const RETRY_BASE: Duration = Duration::from_secs(2);
/// Longest delay between attempts.
const RETRY_MAX: Duration = Duration::from_secs(60);

/// The exchange calls placement needs. `Executor` is the real one; tests use a
/// fake so the whole flow (tick refresh, backoff, pause after rejections) runs offline.
pub trait OrderApi: Send + Sync + 'static {
    /// Tick size the order library will validate against (may be cached).
    fn sdk_tick(&self, token_id: &str) -> impl Future<Output = Result<Decimal>> + Send;
    /// Drop cached tick data and read the current tick from Polymarket.
    fn refresh_tick(&self, token_id: &str) -> impl Future<Output = Result<Decimal>> + Send;
    fn place_buy(&self, token_id: &str, price: Decimal, shares: Decimal)
        -> impl Future<Output = Result<String>> + Send;
    fn cancel(&self, order_id: &str) -> impl Future<Output = Result<bool>> + Send;
    /// Cancel and, when the reply is ambiguous, check Polymarket: `Ok(true)`
    /// once the order is confirmed off the book (cancelled, filled or never
    /// existed), `Err` when that could not be established.
    fn cancel_verified(&self, order_id: &str) -> impl Future<Output = Result<bool>> + Send;
    /// IDs of our open orders on one token (all pages).
    fn open_orders(&self, token_id: &str) -> impl Future<Output = Result<Vec<String>>> + Send;
    /// Cancel several orders; returns the IDs still open afterwards.
    fn cancel_many(&self, order_ids: &[String]) -> impl Future<Output = Result<Vec<String>>> + Send;
}

impl OrderApi for Executor {
    fn sdk_tick(&self, token_id: &str) -> impl Future<Output = Result<Decimal>> + Send {
        Executor::sdk_tick(self, token_id)
    }
    fn refresh_tick(&self, token_id: &str) -> impl Future<Output = Result<Decimal>> + Send {
        Executor::refresh_tick(self, token_id)
    }
    fn place_buy(
        &self,
        token_id: &str,
        price: Decimal,
        shares: Decimal,
    ) -> impl Future<Output = Result<String>> + Send {
        self.place_buy_order(token_id, price, shares)
    }
    fn cancel(&self, order_id: &str) -> impl Future<Output = Result<bool>> + Send {
        self.cancel_order(order_id)
    }
    fn cancel_verified(&self, order_id: &str) -> impl Future<Output = Result<bool>> + Send {
        self.cancel_order_verified(order_id)
    }
    fn open_orders(&self, token_id: &str) -> impl Future<Output = Result<Vec<String>>> + Send {
        self.fetch_open_order_ids(token_id)
    }
    fn cancel_many(&self, order_ids: &[String]) -> impl Future<Output = Result<Vec<String>>> + Send {
        self.cancel_orders(order_ids)
    }
}

/// A cancel still unconfirmed after this long is retried by the 30 s timer.
/// Below the timer's period so each tick picks it up, above
/// `CANCEL_CONFIRM_TIMEOUT` so attempts never overlap.
pub const CANCEL_RETRY_AFTER: Duration = Duration::from_secs(25);
/// Upper bound on one confirmation attempt, shorter than the retry period so
/// retries never overlap.
const CANCEL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(20);
/// Unconfirmed retries in a row before the user is alerted (once per order).
pub const CANCEL_ALERT_AFTER: u32 = 3;
/// After a first "not enough balance", wait this long and try once more: a fill
/// that is still settling can lock collateral for a few seconds.
/// After an unclear placement, wait this long before checking the market's open
/// orders, so an order that was accepted has time to show up.
const RECONCILE_AFTER_UNCLEAR: Duration = Duration::from_secs(2);

/// Check one market's open orders against what the legs track, and cancel any
/// that no leg tracks: an order from an unclear placement, an orphan whose
/// cancel failed, or a leftover of a removed leg. (The startup check does the
/// same for every farmed market.) Never touches a market while a placement on
/// it is in flight, so an order that is just landing isn't mistaken for a
/// stray one. Clears the market's "check before placing" mark once it's clean.
/// Returns how many orders it cancelled.
pub async fn reconcile<A: OrderApi>(api: &A, state: &RwLock<AppState>, alerter: &Alerter, token: &str) -> usize {
    {
        let mut s = state.write().await;
        if s.heartbeat_paused || !s.reconciling.insert(token.to_string()) {
            return 0; // Polymarket unreachable, or already being checked
        }
    }
    let cancelled = reconcile_inner(api, state, alerter, token).await;
    state.write().await.reconciling.remove(token);
    cancelled
}

async fn reconcile_inner<A: OrderApi>(api: &A, state: &RwLock<AppState>, alerter: &Alerter, token: &str) -> usize {
    let placing = |s: &AppState| {
        s.configs
            .iter()
            .any(|c| c.token_id == token && matches!(s.order_status.get(&c.id), Some(OrderStatus::Placing { .. })))
    };
    if placing(&*state.read().await) {
        return 0;
    }
    let open = match api.open_orders(token).await {
        Ok(ids) => ids,
        Err(e) => {
            warn!("Order check for token {}…: couldn't list open orders: {e}", token.get(..8).unwrap_or(token));
            return 0;
        }
    };
    // Decide under the lock, after listing: an order that became tracked
    // meanwhile is excluded; a placement started meanwhile defers the check.
    let (stray, label) = {
        let mut s = state.write().await;
        if placing(&s) {
            return 0;
        }
        let tracked: std::collections::HashSet<&str> = s
            .configs
            .iter()
            .filter(|c| c.token_id == token)
            .filter_map(|c| match s.order_status.get(&c.id) {
                Some(OrderStatus::Live { order_id, .. } | OrderStatus::Cancelling { order_id, .. }) => {
                    Some(order_id.as_str())
                }
                _ => None,
            })
            .collect();
        let stray: Vec<String> = open.into_iter().filter(|id| !tracked.contains(id.as_str())).collect();
        if stray.is_empty() {
            s.reconcile_tokens.remove(token);
            return 0;
        }
        let label = s.configs.iter().find(|c| c.token_id == token).map(|c| c.label.clone());
        (stray, label.unwrap_or_else(|| format!("token {}…", token.get(..8).unwrap_or(token))))
    };
    match api.cancel_many(&stray).await {
        Ok(still_open) => {
            let n = stray.len() - still_open.len();
            if n > 0 {
                alerter.warn(format!(
                    "Cancelled {n} order(s) on '{label}' that no leg was tracking (from a placement that timed out, \
                     a failed cancel, or placed by hand)."
                ));
            }
            if still_open.is_empty() {
                state.write().await.reconcile_tokens.remove(token);
            } else {
                warn!(
                    "Order check on '{label}': {} order(s) still open — retrying on the next check",
                    still_open.len()
                );
            }
            n
        }
        Err(e) => {
            warn!("Order check on '{label}': cancelling {} untracked order(s) failed: {e}", stray.len());
            0
        }
    }
}

/// Placement outages in a row for one leg before the user is told (once).
const OUTAGE_ALERT_AFTER: u32 = 5;
pub const LOW_BALANCE_RETRY: Duration = Duration::from_secs(15);

/// Polymarket refused an order for lack of funds.
pub fn is_low_balance(msg: &str) -> bool {
    msg.to_ascii_lowercase().contains("not enough balance")
}

/// Handle "not enough balance" for a leg that is now Idle (the caller released
/// it). The first time: retry once after [`LOW_BALANCE_RETRY`]. If the leg's
/// next attempt is refused for funds again — however long depth or the
/// two-sided rule delayed it — pause the leg (saved), mark it for the dashboard
/// ("No balance") and alert once. Any other outcome in between (placed,
/// another error, an outage, pause/resume) starts over. `cost` is the order's dollar value, for the
/// message. Never counts as a placement failure.
pub async fn low_balance(state: &RwLock<AppState>, alerter: &Alerter, config_id: &str, cost: Decimal) -> Outcome {
    let mut s = state.write().await;
    // Keyed on the previous attempt, not on time: with a time window, a retry
    // held back past it counted as a fresh "first" refusal and the leg looped.
    let repeat = s.low_balance_since.contains_key(config_id);
    if !repeat {
        s.low_balance_since.insert(config_id.to_string(), std::time::Instant::now());
        s.place_retry_at.insert(config_id.to_string(), Instant::now() + LOW_BALANCE_RETRY);
        warn!("[{config_id}] not enough balance — retrying once in {}s", LOW_BALANCE_RETRY.as_secs());
        return Outcome::Settling;
    }
    s.low_balance_since.remove(config_id);
    s.place_retry_at.remove(config_id);
    let Some(cfg) = s.configs.iter_mut().find(|c| c.id == config_id) else { return Outcome::Superseded };
    cfg.paused = true;
    let label = cfg.label.clone();
    s.low_balance_paused.insert(config_id.to_string());
    let saved = save_markets(&s.markets_file, &s.configs);
    drop(s);
    if let Err(e) = &saved {
        error!("Could not save the pause of '{label}' after not enough balance: {e}");
    }
    alerter.warn(format!(
        "Paused · {label}\nNot enough balance on Polymarket for this order (about ${}). Add funds or lower the size, \
         then Resume.",
        cost.round_dp(2)
    ));
    Outcome::PausedNoBalance
}

/// Cancel `order_id` and wait (bounded) for Polymarket to confirm it is gone.
async fn confirmed_gone<A: OrderApi>(api: &A, order_id: &str) -> bool {
    matches!(tokio::time::timeout(CANCEL_CONFIRM_TIMEOUT, api.cancel_verified(order_id)).await, Ok(Ok(true)))
}

/// Free the leg (Idle) if it is still cancelling exactly `order_id`.
pub fn free_if_cancelling(s: &mut AppState, config_id: &str, order_id: &str) -> bool {
    let still = matches!(s.order_status.get(config_id),
        Some(OrderStatus::Cancelling { order_id: o, .. }) if o == order_id);
    if still {
        s.order_status.insert(config_id.to_string(), OrderStatus::Idle);
    }
    still
}

/// Cancel the order of a leg that was just paused, expired or auto-paused.
/// The leg goes back to Idle only once the cancel is confirmed. If it can't be
/// confirmed the leg stays Cancelling and the timer retries it
/// ([`confirm_stuck_cancel`]); freeing it anyway would let the quoter forget an
/// order that may still be resting while the leg shows as paused.
pub async fn cancel_deactivated<A: OrderApi>(api: &A, state: &RwLock<AppState>, config_id: &str, order_id: &str) {
    if confirmed_gone(api, order_id).await {
        free_if_cancelling(&mut *state.write().await, config_id, order_id);
    } else {
        warn!("Cancel of order {order_id} not confirmed — the timer will retry it");
    }
}

/// Legs whose cancel has been unconfirmed for longer than
/// [`CANCEL_RETRY_AFTER`]. Each one's clock is restarted, which claims the
/// retry so the next timer tick doesn't start a second one while it runs.
pub fn claim_stuck_cancels(s: &mut AppState) -> Vec<(String, String)> {
    let mut stuck = Vec::new();
    for (config_id, status) in s.order_status.iter_mut() {
        if let OrderStatus::Cancelling { order_id, since } = status {
            if since.elapsed() > CANCEL_RETRY_AFTER {
                *since = std::time::Instant::now();
                stuck.push((config_id.clone(), order_id.clone()));
            }
        }
    }
    stuck
}

/// Retry a stuck cancel. Only a confirmed cancel frees the leg; otherwise it
/// stays Cancelling (no new order is placed for it) and, after
/// [`CANCEL_ALERT_AFTER`] tries, the user is told once. Resetting it without
/// confirmation could leave the old order resting beside a new one.
/// Returns whether the order is confirmed gone.
pub async fn confirm_stuck_cancel<A: OrderApi>(
    api: &A,
    state: &RwLock<AppState>,
    alerter: &Alerter,
    config_id: &str,
    order_id: &str,
) -> bool {
    let gone = confirmed_gone(api, order_id).await;
    let mut s = state.write().await;
    let label = s.configs.iter().find(|c| c.id == config_id).map(|c| c.label.clone());
    let alerted_before = matches!(s.cancel_failures.get(config_id),
        Some((o, n)) if o == order_id && *n >= CANCEL_ALERT_AFTER);
    if gone {
        s.cancel_failures.remove(config_id);
        // A Replace whose pre-check failed may have put the leg back to Live
        // with this same (now cancelled) order while the retry ran: free that too,
        // or the leg would show a live order that no longer exists.
        let restored_live = matches!(s.order_status.get(config_id),
            Some(OrderStatus::Live { order_id: o, .. }) if o == order_id);
        if restored_live {
            s.order_status.insert(config_id.to_string(), OrderStatus::Idle);
        }
        let freed = free_if_cancelling(&mut s, config_id, order_id) || restored_live;
        drop(s);
        info!("Cancel of order {order_id} confirmed on retry");
        if freed && alerted_before {
            alerter.info(format!(
                "Cancel confirmed · {}
The leg can quote again.",
                label.unwrap_or_default()
            ));
        }
        return true;
    }
    let still = matches!(s.order_status.get(config_id),
        Some(OrderStatus::Cancelling { order_id: o, .. }) if o == order_id);
    if !still {
        return false; // resolved elsewhere (reconnect, removal) meanwhile
    }
    if s.heartbeat_paused {
        // Polymarket is unreachable and the user already has the heartbeat
        // alert: keep retrying, but don't count tries or add an alert per leg.
        return false;
    }
    let tries = match s.cancel_failures.get_mut(config_id) {
        Some((o, n)) if o == order_id => {
            *n += 1;
            *n
        }
        _ => {
            s.cancel_failures.insert(config_id.to_string(), (order_id.to_string(), 1));
            1
        }
    };
    drop(s);
    warn!("Cancel of order {order_id} still unconfirmed (try {tries}) — leg held until it is");
    if tries == CANCEL_ALERT_AFTER {
        alerter.error(format!(
            "Can't confirm a cancel · {}\nOrder {order_id} may still be resting on Polymarket. Polyfarmer keeps \
             retrying and won't place a new order for this leg until the old one is confirmed gone — check it on \
             Polymarket.",
            label.unwrap_or_default()
        ));
    }
    false
}

/// Cancel an order placed after its leg moved on (the "orphan" of a Replace or
/// placement that lost a race), and alert if Polymarket doesn't confirm it.
pub async fn cancel_orphan<A: OrderApi>(api: &A, alerter: &Alerter, order_id: &str) {
    warn!("Cancelling orphan order {order_id}");
    if !confirmed_gone(api, order_id).await {
        error!("Could not confirm cancel of orphan order {order_id}");
        alerter.error(format!(
            "An order placed while the market was being changed ({order_id}) could not be confirmed cancelled. \
             Check your open orders on Polymarket."
        ));
    }
}

/// Pause a leg after a volatility trigger and save `markets.json` while still
/// holding the write lock, so the saved list always includes the pause and a
/// leg removed meanwhile is never brought back by this save. Returns the leg's
/// label and the save result, or `None` if the leg no longer exists.
pub async fn auto_pause(state: &RwLock<AppState>, config_id: &str) -> Option<(String, Result<()>)> {
    let mut s = state.write().await;
    let cfg = s.configs.iter_mut().find(|c| c.id == config_id)?;
    cfg.paused = true;
    let label = cfg.label.clone();
    let saved = save_markets(&s.markets_file, &s.configs);
    Some((label, saved))
}

/// Delay before the next attempt after `failures` consecutive failures.
pub fn retry_delay(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(16);
    RETRY_BASE.saturating_mul(1u32 << shift).min(RETRY_MAX)
}

/// `price` is a whole number of ticks.
pub fn aligned(price: Decimal, tick: Decimal) -> bool {
    tick > Decimal::ZERO && (price % tick).is_zero()
}

/// Claim the placement slot for a leg: it must be Idle, the heartbeat healthy
/// and any retry delay over. Marks it `Placing` so concurrent events do not
/// place twice, and returns the attempt id that now owns it.
pub fn begin_place(s: &mut AppState, config_id: &str, price: Decimal, now: Instant) -> Option<u64> {
    if s.heartbeat_paused || s.feed_down || s.stopping {
        return None;
    }
    // Re-check the leg itself at claim time: the decision may be a moment old,
    // and the leg paused (dashboard, auto-pause, "No balance"), expired or
    // removed since. Placing then would put an order on a stopped leg, and a
    // removed leg would be left with a "Placing" status nothing clears.
    let c = s.configs.iter().find(|c| c.id == config_id)?;
    if c.paused || c.expires_at <= chrono::Utc::now() || s.low_balance_paused.contains(config_id) {
        return None;
    }
    // A placement on this market went unclear: its open orders are checked first.
    if s.reconcile_tokens.contains(&c.token_id) {
        return None;
    }
    if s.place_retry_at.get(config_id).is_some_and(|at| now < *at) {
        return None;
    }
    let status = s.order_status.entry(config_id.to_string()).or_default();
    if *status != OrderStatus::Idle {
        return None;
    }
    *status = OrderStatus::Placing { price };
    s.next_attempt = s.next_attempt.wrapping_add(1);
    let attempt = s.next_attempt;
    s.place_attempt.insert(config_id.to_string(), attempt);
    Some(attempt)
}

/// One placement call counted in `AppState::placements_in_flight` for as long
/// as it lives. Dropped however the call ends: a reply, an error, a timeout,
/// or its task cancelled or unwinding from a panic.
pub struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    /// Count a placement call now. Taken under the lock that claims the leg
    /// ([`begin_place`]), so no status change can hide the call from shutdown.
    pub fn start(s: &AppState) -> Self {
        s.placements_in_flight.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(&s.placements_in_flight))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Claim a leg's order for cancelling: only while the leg still holds that very
/// order (`Live` with this id, or already `Cancelling` it — the two-sided rule
/// and auto-pause mark it at decision time). Anything else means the state moved
/// on between the decision and now (a replace put up a new order, the leg was
/// removed…); then nothing is touched and the caller does nothing, so a newer
/// order is never overwritten and left untracked.
pub fn begin_cancel(s: &mut AppState, config_id: &str, order_id: &str) -> bool {
    match s.order_status.get(config_id) {
        Some(OrderStatus::Live { order_id: oid, .. }) if oid == order_id => {
            s.order_status.insert(
                config_id.to_string(),
                OrderStatus::Cancelling { order_id: order_id.to_string(), since: std::time::Instant::now() },
            );
            true
        }
        Some(OrderStatus::Cancelling { order_id: oid, .. }) => oid == order_id,
        _ => false,
    }
}

/// A paused or expired leg that had no order when it was evaluated: free it. If
/// a placement finished in the meantime, its order is claimed for cancelling and
/// returned instead (marking it Idle would leave it resting, untracked). A
/// placement still in flight is set Idle; `run_place` then sees it no longer
/// owns the leg and cancels what it gets back.
pub fn settle_unplaced(s: &mut AppState, config_id: &str) -> Option<String> {
    match s.order_status.get(config_id) {
        Some(OrderStatus::Placing { .. }) => {
            s.order_status.insert(config_id.to_string(), OrderStatus::Idle);
            None
        }
        Some(OrderStatus::Live { order_id, .. }) => {
            let oid = order_id.clone();
            begin_cancel(s, config_id, &oid).then_some(oid)
        }
        _ => None,
    }
}

/// This attempt still owns the leg: it is `Placing` and nobody re-claimed it.
fn owns(s: &AppState, config_id: &str, attempt: u64) -> bool {
    matches!(s.order_status.get(config_id), Some(OrderStatus::Placing { .. }))
        && s.place_attempt.get(config_id) == Some(&attempt)
}

/// Back to Idle if this attempt still owns the leg. Returns whether it did.
fn release(s: &mut AppState, config_id: &str, attempt: u64) -> bool {
    if !owns(s, config_id, attempt) {
        return false;
    }
    s.order_status.insert(config_id.to_string(), OrderStatus::Idle);
    s.place_attempt.remove(config_id);
    true
}

/// Adopt a new tick size for every leg on `token_id`: update memory, then save.
/// Returns the labels of the legs that changed. Memory is the source of truth:
/// if the save fails, the next start reads the old tick and corrects it again.
pub async fn apply_tick_change(
    state: &Arc<RwLock<AppState>>,
    alerter: &Alerter,
    token_id: &str,
    new_tick: Decimal,
) -> Vec<String> {
    let changed = {
        let mut s = state.write().await;
        let mut changed = Vec::new();
        for c in s.configs.iter_mut().filter(|c| c.token_id == token_id && c.tick_size != new_tick) {
            changed.push(format!("{} ({} → {})", c.label, cents(c.tick_size), cents(new_tick)));
            c.tick_size = new_tick;
        }
        if changed.is_empty() {
            return changed;
        }
        // Save while still holding the lock, so this snapshot can never be
        // written after a newer change (e.g. a removal) and undo it. Tick
        // changes are rare and the file is small.
        if let Err(e) = save_markets(&s.markets_file, &s.configs) {
            error!("Tick size updated in memory but saving markets failed: {e}");
        }
        changed
    };
    for c in &changed {
        info!("Tick size changed · {c}");
        alerter.info(format!("Tick size changed · {c}. Prices now follow the new tick."));
    }
    changed
}

fn cents(tick: Decimal) -> String {
    format!("{}¢", (tick * Decimal::ONE_HUNDRED).normalize())
}

fn mentions_tick(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("tick size") || e.contains("tick_size")
}

/// Leg identity for messages.
fn names(s: &AppState, config_id: &str) -> (String, String) {
    s.configs
        .iter()
        .find(|c| c.id == config_id)
        .map(|c| (c.label.clone(), c.token_label.clone()))
        .unwrap_or_else(|| (config_id.to_string(), "?".to_string()))
}

/// What one attempt ended in (returned for tests and logs).
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Placed,
    /// The tick changed and the price no longer fits: re-quote, no failure.
    Requote,
    /// Not enough balance for the first time: one retry after
    /// [`LOW_BALANCE_RETRY`] (collateral may be locked by a settling fill), no failure.
    Settling,
    /// Not enough balance again: the leg was paused and the user told.
    PausedNoBalance,
    Failed {
        failures: u32,
    },
    /// Rejected [`MAX_PLACE_FAILURES`] times in a row: the leg was paused.
    PausedAfterRejections,
    /// A network/Polymarket outage, not counted as a failure.
    Outage {
        tries: u32,
    },
    /// Something reset the slot while we were placing (pause, remove, reconnect).
    Superseded,
}

/// Result of making the order library and the config agree on the tick.
#[derive(Debug, PartialEq, Eq)]
pub enum TickCheck {
    /// The price is valid for the current tick: go ahead.
    Ok,
    /// The tick changed and the price no longer fits it: quote again.
    Requote,
    /// The tick could not be read.
    Error(String),
}

/// Compare the order library's tick with the config's. Only on a mismatch,
/// read the real tick from Polymarket, adopt it, and say whether `price`
/// still fits. Costs no request when they agree (the library's value is cached).
pub async fn check_tick<A: OrderApi>(
    api: &A,
    state: &Arc<RwLock<AppState>>,
    alerter: &Alerter,
    token_id: &str,
    cfg_tick: Decimal,
    price: Decimal,
) -> TickCheck {
    match api.sdk_tick(token_id).await {
        Ok(sdk) if sdk == cfg_tick => TickCheck::Ok,
        Ok(sdk) => {
            warn!("Tick mismatch on {token_id}: order library {sdk}, config {cfg_tick}; reading the current tick");
            match api.refresh_tick(token_id).await {
                Ok(real) => {
                    if real != cfg_tick {
                        apply_tick_change(state, alerter, token_id, real).await;
                    }
                    if aligned(price, real) {
                        TickCheck::Ok
                    } else {
                        TickCheck::Requote
                    }
                }
                Err(e) => TickCheck::Error(format!("could not read tick size: {e}")),
            }
        }
        Err(e) => TickCheck::Error(format!("could not read tick size: {e}")),
    }
}

/// Place one order for a leg that `begin_place` has claimed with `attempt`.
#[allow(clippy::too_many_arguments)]
pub async fn run_place<A: OrderApi>(
    api: Arc<A>,
    state: Arc<RwLock<AppState>>,
    alerter: Arc<Alerter>,
    config_id: String,
    attempt: u64,
    token_id: String,
    order_size: Decimal,
    price: Decimal,
    cfg_tick: Decimal,
) -> Outcome {
    // 1. Make sure the order library and the config agree on the tick.
    match check_tick(&*api, &state, &alerter, &token_id, cfg_tick, price).await {
        TickCheck::Ok => {}
        TickCheck::Requote => {
            let mut s = state.write().await;
            return if release(&mut s, &config_id, attempt) { Outcome::Requote } else { Outcome::Superseded };
        }
        TickCheck::Error(msg) => return fail(&state, &alerter, &config_id, attempt, msg, None).await,
    }

    // 2. Place.
    let shares = match Executor::shares_from_usd(order_size, price) {
        Ok(s) => s,
        Err(e) => {
            error!("Place: invalid price {price}: {e}");
            let mut s = state.write().await;
            release(&mut s, &config_id, attempt);
            return Outcome::Superseded;
        }
    };
    match api.place_buy(&token_id, price, shares).await {
        Ok(order_id) => {
            let mut s = state.write().await;
            let still_ours = owns(&s, &config_id, attempt)
                && matches!(s.order_status.get(&config_id), Some(OrderStatus::Placing { price: p }) if *p == price);
            if !still_ours {
                drop(s);
                warn!("State changed during placement — cancelling orphan {order_id}");
                // Verified (checks Polymarket on an ambiguous reply), one alert text.
                cancel_orphan(&*api, &alerter, &order_id).await;
                return Outcome::Superseded;
            }
            s.placed_orders
                .insert(order_id.clone(), PlacedOrder { token_id, price, usd: order_size, released_at: None });
            s.order_status.insert(config_id.clone(), OrderStatus::Live { order_id, price });
            s.place_attempt.remove(&config_id);
            s.place_failures.remove(&config_id);
            s.place_retry_at.remove(&config_id);
            s.low_balance_since.remove(&config_id);
            s.place_outage.remove(&config_id);
            let replaced = s.repegging.remove(&config_id);
            let (label, token_label) = names(&s, &config_id);
            drop(s);
            alerter.info(format!(
                "Order {} · {label}\nBUY {shares} {token_label} shares @ {}¢ (${order_size})",
                if replaced { "replaced" } else { "placed" },
                price * Decimal::ONE_HUNDRED
            ));
            Outcome::Placed
        }
        Err(e) => {
            let msg = e.to_string();
            // A network error, timeout or 5xx may come after the order reached
            // Polymarket. Check that market's open orders before any leg there
            // places again, so an order that did land isn't left untracked
            // beside a second one.
            if is_transient(&msg) {
                state.write().await.reconcile_tokens.insert(token_id.clone());
                let (api, state, alerter, token) =
                    (Arc::clone(&api), Arc::clone(&state), Arc::clone(&alerter), token_id.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(RECONCILE_AFTER_UNCLEAR).await;
                    reconcile(&*api, &state, &alerter, &token).await;
                });
            }
            // Not enough funds (or collateral briefly locked by a settling fill):
            // retry once, then pause the leg and say so. Never a "failure".
            if is_low_balance(&msg) {
                let released = {
                    let mut s = state.write().await;
                    release(&mut s, &config_id, attempt)
                };
                return if released {
                    low_balance(&state, &alerter, &config_id, shares * price).await
                } else {
                    Outcome::Superseded
                };
            }
            // The exchange rejected the price over the tick: re-read it once.
            if mentions_tick(&msg) {
                if let Ok(real) = api.refresh_tick(&token_id).await {
                    if real != cfg_tick {
                        apply_tick_change(&state, &alerter, &token_id, real).await;
                        let mut s = state.write().await;
                        return if release(&mut s, &config_id, attempt) {
                            Outcome::Requote
                        } else {
                            Outcome::Superseded
                        };
                    }
                }
            }
            fail(&state, &alerter, &config_id, attempt, msg, Some(shares)).await
        }
    }
}

/// Polymarket refused the order as under the market's minimum size (its
/// wording: "Size (4.88) lower than the minimum: 5").
pub fn is_too_small(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("lower than the minimum") || m.contains("below minimum")
}

/// Why a leg was paused after [`MAX_PLACE_FAILURES`] rejections, for the
/// alert. A minimum-size rejection is said plainly first, with the order's
/// shares (`None` when no order was built), then in Polymarket's words. The
/// engine doesn't know the market's minimum, so it isn't named.
fn pause_reason(msg: &str, shares: Option<Decimal>) -> String {
    match shares {
        Some(shares) if is_too_small(msg) => format!(
            "Order too small: {} shares, below Polymarket's minimum. Raise the size in Edit.\nPolymarket said: {msg}",
            shares.normalize()
        ),
        _ => format!(
            "{MAX_PLACE_FAILURES} placements in a row were rejected. Last reason: {msg}\nFix the leg (Edit) and Resume."
        ),
    }
}

/// A placement failure that says nothing about the order itself: the network,
/// a timeout, or Polymarket busy or down (5xx, 429, session sync). The SDK
/// reports network errors as `Internal: …` and HTTP ones as `Status: error(503
/// …)`; a 4xx or a validation error is a real rejection.
pub fn is_transient(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.starts_with("internal:")
        || m.starts_with("synchronization")
        || m.contains("error(5")
        || m.contains("error(429")
        || m.contains("timed out")
        || m.contains("timeout")
        || m.contains("connection")
        || m.contains("error sending request")
}

/// An outage, not the order's fault: retry with a growing wait (2 s doubling
/// to a minute), never counted as a failure, so it can't pause the leg. One
/// alert if it keeps happening.
async fn outage(state: &Arc<RwLock<AppState>>, alerter: &Alerter, config_id: &str, attempt: u64, msg: &str) -> Outcome {
    let (n, label) = {
        let mut s = state.write().await;
        if !release(&mut s, config_id, attempt) {
            return Outcome::Superseded;
        }
        s.low_balance_since.remove(config_id); // a different outcome: start over
        let n = {
            let n = s.place_outage.entry(config_id.to_string()).or_insert(0);
            *n += 1;
            *n
        };
        s.place_retry_at.insert(config_id.to_string(), Instant::now() + retry_delay(n));
        (n, names(&s, config_id).0)
    };
    warn!("Placement for '{label}' failed for a network/Polymarket reason ({n} in a row) — retrying: {msg}");
    if n == OUTAGE_ALERT_AFTER {
        alerter.warn(format!(
            "Can't place · {label}\nPolymarket or the network keeps failing ({msg}). Still retrying every minute."
        ));
    }
    Outcome::Outage { tries: n }
}

/// A real rejection: back off, and after [`MAX_PLACE_FAILURES`] in a row pause
/// the leg (saved, under the lock) and say why. Never deletes it: a leg's
/// settings are the user's, and a rejection is usually fixable (size, price,
/// funds) — before, three in a row removed the leg for good. `shares` is the
/// rejected order's size, `None` when it failed before an order was built.
async fn fail(
    state: &Arc<RwLock<AppState>>,
    alerter: &Alerter,
    config_id: &str,
    attempt: u64,
    msg: String,
    shares: Option<Decimal>,
) -> Outcome {
    if is_transient(&msg) {
        return outage(state, alerter, config_id, attempt, &msg).await;
    }
    error!("Place order failed: {msg}");
    alerter.error(format!("Order placement failed: {msg}"));
    let mut s = state.write().await;
    if !release(&mut s, config_id, attempt) {
        return Outcome::Superseded;
    }
    s.low_balance_since.remove(config_id); // a different outcome: start over
    let failures = {
        let f = s.place_failures.entry(config_id.to_string()).or_insert(0);
        *f += 1;
        *f
    };
    s.place_retry_at.insert(config_id.to_string(), Instant::now() + retry_delay(failures));
    if failures < MAX_PLACE_FAILURES {
        return Outcome::Failed { failures };
    }
    let Some(cfg) = s.configs.iter_mut().find(|c| c.id == config_id) else { return Outcome::Superseded };
    cfg.paused = true;
    let label = cfg.label.clone();
    s.place_failures.remove(config_id);
    s.place_retry_at.remove(config_id);
    let saved = save_markets(&s.markets_file, &s.configs);
    drop(s);
    if let Err(e) = &saved {
        error!("Could not save the pause of '{label}' after {MAX_PLACE_FAILURES} rejected placements: {e}");
        alerter.error(format!(
            "'{label}' is paused but the pause could not be saved: {e}\nIt would resume after a restart."
        ));
    }
    alerter.error(format!("Paused · {label}\n{}", pause_reason(&msg, shares)));
    Outcome::PausedAfterRejections
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::*;
    use crate::types::MarketConfig;
    use rust_decimal_macros::dec;

    async fn attempt(e: &Env, api: &Arc<Fake>, id: &str, price: Decimal, now: Instant) -> Option<Outcome> {
        let (claimed, tick) = {
            let mut s = e.state.write().await;
            let tick = s.configs.iter().find(|c| c.id == id).map(|c| c.tick_size).unwrap_or_default();
            (begin_place(&mut s, id, price, now), tick)
        };
        Some(run(e, api, id, claimed?, price, tick).await)
    }

    async fn run(e: &Env, api: &Arc<Fake>, id: &str, attempt: u64, price: Decimal, tick: Decimal) -> Outcome {
        run_place(
            Arc::clone(api),
            Arc::clone(&e.state),
            Arc::clone(&e.alerter),
            id.to_string(),
            attempt,
            TOKEN.to_string(),
            dec!(150),
            price,
            tick,
        )
        .await
    }

    fn saved_ticks(e: &Env) -> Vec<Decimal> {
        let file = e.dir.join("markets.json");
        let legs: Vec<MarketConfig> = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        legs.iter().map(|c| c.tick_size).collect()
    }

    // ── part 1: the reported bug ─────────────────────────────────────────

    #[tokio::test]
    async fn fake_reproduces_the_reported_error_without_the_fix() {
        // Order library cached 0.01, market and config are on 0.001.
        let api = Fake::new(dec!(0.01), dec!(0.001));
        let err = api.place_buy(TOKEN, dec!(0.213), dec!(704)).await.unwrap_err().to_string();
        assert!(err.contains("Price 0.213 has 3 decimal places"), "{err}");
    }

    #[tokio::test]
    async fn stale_order_library_tick_is_refreshed_and_the_order_placed() {
        let e = env(vec![leg("a", dec!(0.001))]);
        let api = Fake::new(dec!(0.01), dec!(0.001));
        let out = attempt(&e, &api, "a", dec!(0.213), Instant::now()).await;
        assert_eq!(out, Some(Outcome::Placed));
        assert_eq!(*api.refreshes.lock().unwrap(), 1);
        assert_eq!(*api.placed.lock().unwrap(), vec![dec!(0.213)]);
        let s = e.state.read().await;
        assert!(matches!(s.order_status.get("a"), Some(OrderStatus::Live { .. })));
        assert!(!s.place_failures.contains_key("a"), "a tick refresh is not a failure");
        assert_eq!(s.configs[0].tick_size, dec!(0.001), "config was already right");
    }

    #[tokio::test]
    async fn matching_ticks_place_without_any_refresh() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Placed));
        assert_eq!(*api.refreshes.lock().unwrap(), 0, "no extra request in the normal case");
    }

    #[tokio::test]
    async fn finer_tick_is_adopted_saved_and_the_order_placed() {
        // Config and library think 1¢; the library refetched 0.1¢ elsewhere.
        let e = env(vec![leg("a", dec!(0.01)), leg("b", dec!(0.01))]);
        let api = Fake::new(dec!(0.001), dec!(0.001));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Placed));
        let s = e.state.read().await;
        assert!(s.configs.iter().all(|c| c.tick_size == dec!(0.001)), "every leg on the token follows");
        drop(s);
        assert_eq!(saved_ticks(&e), vec![dec!(0.001), dec!(0.001)], "and it is saved");
    }

    #[tokio::test]
    async fn coarser_tick_rejected_by_the_exchange_requotes_without_a_failure() {
        // Everything local says 0.1¢, the market moved to 1¢: 0.213 is now invalid.
        let e = env(vec![leg("a", dec!(0.001))]);
        let api = Fake::new(dec!(0.001), dec!(0.01));
        assert_eq!(attempt(&e, &api, "a", dec!(0.213), Instant::now()).await, Some(Outcome::Requote));
        let s = e.state.read().await;
        assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle), "free to quote again at once");
        assert!(!s.place_failures.contains_key("a"));
        assert!(!s.place_retry_at.contains_key("a"), "no backoff for a tick change");
        assert_eq!(s.configs[0].tick_size, dec!(0.01));
        drop(s);
        assert_eq!(saved_ticks(&e), vec![dec!(0.01)]);
        // The next quote is on the new tick and goes through.
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Placed));
    }

    // ── part 3: backoff, outages and pause after rejections ─────────────

    #[test]
    fn retry_delay_doubles_and_is_capped() {
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(2), Duration::from_secs(4));
        assert_eq!(retry_delay(3), Duration::from_secs(8));
        assert_eq!(retry_delay(10), Duration::from_secs(60));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(60), "no overflow");
        assert_eq!(retry_delay(0), Duration::from_secs(2));
    }

    /// Three real rejections in a row (spaced by the backoff) pause the leg and
    /// save that — it used to be deleted for good. Other legs are untouched.
    #[tokio::test(start_paused = true)]
    async fn rejections_back_off_and_the_leg_is_paused_not_removed_after_three() {
        use tokio::time::advance;
        let e = env(vec![leg("a", dec!(0.01)), leg("keep", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        for _ in 0..3 {
            api.script(Err("Status: error(400 Bad Request) making POST call to /order with invalid amount"));
        }
        let try_now = || attempt(&e, &api, "a", dec!(0.21), Instant::now());
        assert_eq!(try_now().await, Some(Outcome::Failed { failures: 1 }));
        // A burst of book updates in the next 2 s: all held back.
        for step in [0, 10, 490, 1400] {
            advance(Duration::from_millis(step)).await;
            assert_eq!(try_now().await, None);
        }
        advance(Duration::from_millis(200)).await; // 2.1 s after the failure
        assert_eq!(try_now().await, Some(Outcome::Failed { failures: 2 }));
        advance(Duration::from_secs(3)).await;
        assert_eq!(try_now().await, None, "the second delay is 4 s");
        advance(Duration::from_millis(1100)).await;
        assert_eq!(try_now().await, Some(Outcome::PausedAfterRejections));
        let s = e.state.read().await;
        assert_eq!(s.configs.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["a", "keep"], "nothing deleted");
        assert!(s.configs[0].paused && !s.configs[1].paused);
        assert!(!s.place_failures.contains_key("a") && !s.place_retry_at.contains_key("a"), "a fresh start on Resume");
        drop(s);
        let saved: Vec<MarketConfig> =
            serde_json::from_str(&std::fs::read_to_string(e.dir.join("markets.json")).unwrap()).unwrap();
        assert!(saved.len() == 2 && saved[0].paused, "the pause is saved");
        let text = alerts(&e);
        assert!(text.contains("Paused · leg a") && text.contains("invalid amount"), "{text}");
    }

    /// The size is in dollars, so a re-peg to a higher price can take an order
    /// under Polymarket's minimum in shares ($2 at 41¢ is 4.88 shares). After
    /// three such rejections the pause alert says so plainly first, with the
    /// order's real shares, then keeps Polymarket's own words.
    #[tokio::test(start_paused = true)]
    async fn a_pause_for_an_order_under_the_minimum_says_so_plainly() {
        let mut a = leg("a", dec!(0.01));
        a.order_size = dec!(2);
        let e = env(vec![a]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        let raw = r#"Status: error(400 Bad Request) making POST call to /order with {"error":"order 0x1 is invalid. Size (4.88) lower than the minimum: 5"}"#;
        for _ in 0..3 {
            api.script(Err(raw));
        }
        // The `attempt` helper places $150; this leg's order is $2.
        for expected in
            [Outcome::Failed { failures: 1 }, Outcome::Failed { failures: 2 }, Outcome::PausedAfterRejections]
        {
            let claimed = begin_place(&mut *e.state.write().await, "a", dec!(0.41), Instant::now()).unwrap();
            let (state, alerter) = (Arc::clone(&e.state), Arc::clone(&e.alerter));
            let outcome = run_place(
                Arc::clone(&api),
                state,
                alerter,
                "a".into(),
                claimed,
                TOKEN.into(),
                dec!(2),
                dec!(0.41),
                dec!(0.01),
            )
            .await;
            assert_eq!(outcome, expected);
            tokio::time::advance(Duration::from_secs(61)).await;
        }
        let messages: Vec<String> =
            alerts(&e).lines().map(|l| serde_json::from_str::<crate::types::Alert>(l).unwrap().message).collect();
        assert_eq!(
            messages.last().map(String::as_str),
            Some(
                format!(
                    "Paused · leg a\nOrder too small: 4.88 shares, below Polymarket's minimum. Raise the size in \
                     Edit.\nPolymarket said: {raw}"
                )
                .as_str()
            )
        );
    }

    /// Network errors, timeouts, 5xx and 429 say nothing about the order: they
    /// back off (2 s doubling to a minute) but never count, so an outage can't
    /// pause or delete legs. One alert if it keeps happening.
    #[tokio::test(start_paused = true)]
    async fn an_outage_backs_off_without_counting() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        let errors = [
            "Internal: error sending request for url (https://clob.polymarket.com/order)",
            "Status: error(503 Service Unavailable) making POST call to /order with ",
            "Status: error(429 Too Many Requests) making POST call to /order with ",
            "operation timed out",
            "Status: error(502 Bad Gateway) making POST call to /order with ",
            "connection reset by peer",
        ];
        for (i, err) in errors.iter().enumerate() {
            api.script(Err(err));
            let n = i as u32 + 1;
            assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Outage { tries: n }));
            // Wait out the backoff; meanwhile the market's order check runs
            // (an unclear placement blocks the leg until it has).
            tokio::time::sleep(retry_delay(n) + Duration::from_millis(10)).await;
        }
        {
            let s = e.state.read().await;
            assert!(s.place_failures.is_empty() && !s.configs[0].paused, "never counted, never paused");
        }
        assert_eq!(alerts(&e).matches("Can't place · leg a").count(), 1, "told once");
        // Back up: placed, and the outage count is cleared.
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Placed));
        assert!(e.state.read().await.place_outage.is_empty());
    }

    /// A placement call is counted until it ends, however it ends: returned,
    /// its task panicking (unwinding) or cancelled.
    #[tokio::test]
    async fn a_placement_call_is_counted_until_it_ends_however_it_ends() {
        let s = AppState::new(std::path::PathBuf::from("unused.json"));
        let count = || s.placements_in_flight.load(Ordering::SeqCst);
        let (first, second) = (InFlight::start(&s), InFlight::start(&s));
        assert_eq!(count(), 2);
        drop(first);
        assert_eq!(count(), 1);
        drop(second);
        assert_eq!(count(), 0);

        let call = InFlight::start(&s);
        let panicked = tokio::spawn(async move {
            let _call = call;
            panic!("the call panicked");
        })
        .await;
        assert!(panicked.is_err_and(|e| e.is_panic()));
        assert_eq!(count(), 0, "released while unwinding");

        let call = InFlight::start(&s);
        let task = tokio::spawn(async move {
            let _call = call;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(count(), 1);
        task.abort();
        assert!(task.await.is_err_and(|e| e.is_cancelled()));
        assert_eq!(count(), 0, "released when its task is cancelled");
    }

    /// The order check cancels only what no leg tracks, and clears the
    /// market's "check before placing" mark once it's clean.
    #[tokio::test]
    async fn the_order_check_cancels_only_untracked_orders() {
        let e = env(vec![leg("a", dec!(0.01)), leg("b", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        {
            let mut s = e.state.write().await;
            s.order_status.insert("a".into(), OrderStatus::Live { order_id: "mine-a".into(), price: dec!(0.2) });
            s.order_status.insert(
                "b".into(),
                OrderStatus::Cancelling { order_id: "mine-b".into(), since: std::time::Instant::now() },
            );
            s.reconcile_tokens.insert(TOKEN.into());
        }
        api.open
            .lock()
            .unwrap()
            .insert(TOKEN.into(), ["mine-a", "mine-b", "stray-1", "stray-2"].map(String::from).to_vec());
        assert_eq!(reconcile(&*api, &e.state, &e.alerter, TOKEN).await, 2);
        assert_eq!(*api.bulk_cancelled.lock().unwrap(), vec!["stray-1".to_string(), "stray-2".to_string()]);
        let s = e.state.read().await;
        assert!(s.reconcile_tokens.is_empty() && s.reconciling.is_empty());
        assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Live { order_id: "mine-a".into(), price: dec!(0.2) }));
        drop(s);
        assert!(alerts(&e).contains("Cancelled 2 order(s) on 'leg a' that no leg was tracking"));
        // Clean now: nothing more to do.
        assert_eq!(reconcile(&*api, &e.state, &e.alerter, TOKEN).await, 0);
    }

    /// A market with a placement in flight is left alone (its order may be
    /// landing and not tracked yet); a failed listing changes nothing.
    #[tokio::test]
    async fn the_order_check_never_touches_a_landing_order_and_survives_errors() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.open.lock().unwrap().insert(TOKEN.into(), vec!["landing".into()]);
        {
            let mut s = e.state.write().await;
            s.order_status.insert("a".into(), OrderStatus::Placing { price: dec!(0.2) });
            s.reconcile_tokens.insert(TOKEN.into());
        }
        assert_eq!(reconcile(&*api, &e.state, &e.alerter, TOKEN).await, 0);
        assert!(api.bulk_cancelled.lock().unwrap().is_empty());
        assert!(e.state.read().await.reconcile_tokens.contains(TOKEN), "still to check");

        e.state.write().await.order_status.insert("a".into(), OrderStatus::Idle);
        *api.list_fails.lock().unwrap() = true;
        assert_eq!(reconcile(&*api, &e.state, &e.alerter, TOKEN).await, 0);
        assert!(e.state.read().await.reconcile_tokens.contains(TOKEN), "unknown: still to check");
        assert!(e.state.read().await.reconciling.is_empty(), "the check slot is freed");
    }

    /// The bug behind it: a placement that times out (or hits a network
    /// error / 5xx) may still have reached Polymarket. Its leg must not place
    /// again until that market's open orders are checked — then the stray order
    /// is cancelled and placing resumes.
    #[tokio::test(start_paused = true)]
    async fn an_unclear_placement_blocks_the_market_until_its_orders_are_checked() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.script(Err("placing the order timed out after 15s"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Outage { tries: 1 }));
        // It did land, though.
        api.open.lock().unwrap().insert(TOKEN.into(), vec!["landed-anyway".into()]);
        let later = Instant::now() + Duration::from_secs(60);
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), later).await, None, "blocked until checked");
        // The check runs shortly after: the stray order is cancelled, placing resumes.
        tokio::time::sleep(RECONCILE_AFTER_UNCLEAR + Duration::from_millis(50)).await;
        assert_eq!(*api.bulk_cancelled.lock().unwrap(), vec!["landed-anyway".to_string()]);
        assert!(e.state.read().await.reconcile_tokens.is_empty());
        let later = Instant::now() + Duration::from_secs(60);
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), later).await, Some(Outcome::Placed));
    }

    #[test]
    fn outages_and_rejections_are_told_apart() {
        for t in [
            "Internal: error sending request",
            "Status: error(500 Internal Server Error) making POST call",
            "Status: error(429 Too Many Requests) making POST call",
            "Synchronization: …",
            "request timed out",
        ] {
            assert!(is_transient(t), "{t}");
        }
        for r in [
            "Status: error(400 Bad Request) making POST call to /order with invalid order",
            "Validation: invalid: price 0.205 has 3 decimal places",
            "order rejected: order size below minimum",
            "Geoblock: trading restricted in your region",
        ] {
            assert!(!is_transient(r), "{r}");
        }
    }

    /// Minimum-size rejections are told apart from the rest, and only they
    /// change the pause alert's reason; every other one reads as before.
    #[test]
    fn only_a_minimum_size_rejection_is_explained_plainly() {
        let polymarket = "order 0x1 is invalid. Size (4.88) lower than the minimum: 5";
        for t in [polymarket, "order rejected: order size below minimum"] {
            assert!(is_too_small(t), "{t}");
        }
        for t in [
            "order 0.213 is invalid: breaks minimum tick size rule: 0.01",
            "not enough balance / allowance",
            "Status: error(400 Bad Request) making POST call to /order with invalid amount",
        ] {
            assert!(!is_too_small(t), "{t}");
            assert_eq!(
                pause_reason(t, Some(dec!(4.88))),
                format!("3 placements in a row were rejected. Last reason: {t}\nFix the leg (Edit) and Resume.")
            );
        }
        assert_eq!(
            pause_reason(polymarket, Some(dec!(4.88))),
            format!(
                "Order too small: 4.88 shares, below Polymarket's minimum. Raise the size in Edit.\nPolymarket said: \
                 {polymarket}"
            )
        );
        assert!(pause_reason(polymarket, Some(dec!(2.60))).starts_with("Order too small: 2.6 shares, below"));
        // No order was built (the tick couldn't be read): nothing to explain.
        assert_eq!(
            pause_reason(polymarket, None),
            format!("3 placements in a row were rejected. Last reason: {polymarket}\nFix the leg (Edit) and Resume.")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_clears_the_failure_count_and_the_delay() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.script(Err("Status: error(400 Bad Request) making POST call to /order with invalid"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Failed { failures: 1 }));
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Placed));
        let s = e.state.read().await;
        assert!(s.place_failures.is_empty() && s.place_retry_at.is_empty());
    }

    /// "Not enough balance": the first time it retries once after a short wait
    /// (collateral can be locked while a fill settles); again within the window
    /// it pauses the leg, saves that, marks it for the dashboard and alerts once.
    /// It is never a placement failure.
    #[tokio::test]
    async fn not_enough_balance_retries_once_then_pauses() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        let now = Instant::now();
        api.script(Err("not enough balance / allowance"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), now).await, Some(Outcome::Settling));
        {
            let s = e.state.read().await;
            assert!(s.place_failures.is_empty(), "not a failure");
            assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle));
            assert!(s
                .place_retry_at
                .get("a")
                .is_some_and(|t| *t >= now + LOW_BALANCE_RETRY - Duration::from_millis(50)));
            assert!(!s.configs[0].paused && s.low_balance_paused.is_empty());
        }
        // The retry waits.
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, None, "held until the retry time");

        // Second refusal: paused, saved, shown, one alert.
        api.script(Err("not enough balance / allowance"));
        let later = Instant::now() + LOW_BALANCE_RETRY + Duration::from_millis(10);
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), later).await, Some(Outcome::PausedNoBalance));
        {
            let s = e.state.read().await;
            assert!(s.configs[0].paused && s.low_balance_paused.contains("a"));
            assert!(s.place_failures.is_empty() && s.low_balance_since.is_empty());
            assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle), "no order left");
        }
        let saved: Vec<MarketConfig> =
            serde_json::from_str(&std::fs::read_to_string(e.dir.join("markets.json")).unwrap()).unwrap();
        assert!(saved[0].paused, "the pause is saved");
        let text = alerts(&e);
        assert_eq!(text.matches("Not enough balance on Polymarket").count(), 1, "{text}");
        assert!(!text.contains("Order placement failed"), "{text}");
    }

    /// The bug: the second refusal only paused within 2 minutes of the first.
    /// When depth or the two-sided rule held the retry back longer, it counted
    /// as a fresh "first" refusal, and the leg cycled forever without a "No
    /// balance" status. Now the next attempt's refusal pauses, however late.
    #[tokio::test(start_paused = true)]
    async fn a_late_second_refusal_still_pauses() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.script(Err("not enough balance / allowance"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Settling));
        tokio::time::advance(Duration::from_secs(600)).await; // held back for 10 minutes
        api.script(Err("not enough balance / allowance"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::PausedNoBalance));
        assert!(e.state.read().await.low_balance_paused.contains("a"));
    }

    /// Another outcome between two refusals starts over: the next refusal is a
    /// "first" one again (retry, not pause).
    #[tokio::test(start_paused = true)]
    async fn another_outcome_between_refusals_starts_over() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.script(Err("not enough balance / allowance"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Settling));
        tokio::time::sleep(LOW_BALANCE_RETRY + Duration::from_millis(10)).await;
        api.script(Err("Status: error(400 Bad Request) making POST call to /order with invalid"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Failed { failures: 1 }));
        tokio::time::sleep(Duration::from_secs(3)).await;
        api.script(Err("not enough balance / allowance"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Settling));
        assert!(!e.state.read().await.configs[0].paused);
    }

    #[tokio::test]
    async fn a_successful_place_forgets_an_earlier_low_balance() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.script(Err("not enough balance / allowance"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Settling));
        api.script(Ok(()));
        let later = Instant::now() + LOW_BALANCE_RETRY + Duration::from_millis(10);
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), later).await, Some(Outcome::Placed));
        assert!(e.state.read().await.low_balance_since.is_empty());
        // So a later refusal is a fresh first one: retry, not pause.
        {
            let mut s = e.state.write().await;
            s.order_status.insert("a".into(), OrderStatus::Idle);
        }
        api.script(Err("not enough balance / allowance"));
        let much_later = later + Duration::from_secs(1);
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), much_later).await, Some(Outcome::Settling));
        assert!(!e.state.read().await.configs[0].paused);
    }

    fn live(id: &str) -> OrderStatus {
        OrderStatus::Live { order_id: id.into(), price: dec!(0.2) }
    }
    fn cancelling_status(id: &str) -> OrderStatus {
        OrderStatus::Cancelling { order_id: id.into(), since: std::time::Instant::now() }
    }

    /// A cancel only claims the order the leg still holds: never a newer one put
    /// up in between (that one would be overwritten and left resting untracked).
    #[tokio::test]
    async fn a_cancel_only_claims_the_order_the_leg_still_holds() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let mut s = e.state.write().await;
        s.order_status.insert("a".into(), live("old"));
        assert!(begin_cancel(&mut s, "a", "old"));
        assert!(matches!(s.order_status.get("a"), Some(OrderStatus::Cancelling { order_id, .. }) if order_id == "old"));
        // Already being cancelled (the two-sided rule marks it at decision time): fine.
        assert!(begin_cancel(&mut s, "a", "old"));
        // A replace put up "new" meanwhile: the decision about "old" touches nothing.
        s.order_status.insert("a".into(), live("new"));
        assert!(!begin_cancel(&mut s, "a", "old"));
        assert_eq!(s.order_status.get("a"), Some(&live("new")));
        s.order_status.insert("a".into(), cancelling_status("new"));
        assert!(!begin_cancel(&mut s, "a", "old"));
        for st in [OrderStatus::Idle, OrderStatus::Placing { price: dec!(0.2) }] {
            s.order_status.insert("a".into(), st.clone());
            assert!(!begin_cancel(&mut s, "a", "old"));
            assert_eq!(s.order_status.get("a"), Some(&st), "untouched");
        }
        assert!(!begin_cancel(&mut s, "gone", "old"), "a removed leg");
    }

    /// A paused/expired leg with no order at decision time: a placement still in
    /// flight is freed, but one that landed meanwhile is cancelled — never marked
    /// Idle while its order rests on the book.
    #[tokio::test]
    async fn settling_a_paused_leg_cancels_an_order_that_landed_meanwhile() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let mut s = e.state.write().await;
        s.order_status.insert("a".into(), OrderStatus::Placing { price: dec!(0.2) });
        assert_eq!(settle_unplaced(&mut s, "a"), None);
        assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle));

        s.order_status.insert("a".into(), live("landed"));
        assert_eq!(settle_unplaced(&mut s, "a").as_deref(), Some("landed"));
        assert!(
            matches!(s.order_status.get("a"), Some(OrderStatus::Cancelling { order_id, .. }) if order_id == "landed")
        );

        // Already cancelling, or idle: nothing to do.
        assert_eq!(settle_unplaced(&mut s, "a"), None);
        s.order_status.insert("a".into(), OrderStatus::Idle);
        assert_eq!(settle_unplaced(&mut s, "a"), None);
        assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle));
    }

    #[test]
    fn low_balance_is_recognised_whatever_the_case() {
        assert!(is_low_balance("not enough balance / allowance"));
        assert!(is_low_balance("PolyApi: Not Enough Balance"));
        assert!(!is_low_balance("invalid tick size"));
    }

    /// A decision made a moment ago must not place on a leg that has since been
    /// paused, expired or removed (the claim re-checks the leg itself).
    #[tokio::test]
    async fn the_claim_rechecks_a_paused_expired_or_removed_leg() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let mut s = e.state.write().await;
        let claim = |s: &mut AppState, id: &str| begin_place(s, id, dec!(0.2), Instant::now());
        s.configs[0].paused = true;
        assert_eq!(claim(&mut s, "a"), None, "paused since the decision");
        s.configs[0].paused = false;
        s.low_balance_paused.insert("a".into());
        assert_eq!(claim(&mut s, "a"), None, "stopped for lack of funds");
        s.low_balance_paused.clear();
        s.configs[0].expires_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        assert_eq!(claim(&mut s, "a"), None, "expired");
        assert_eq!(s.order_status.get("a"), None, "none of these left a status behind");
        // Removed: no phantom "Placing" entry for a leg that no longer exists.
        assert_eq!(claim(&mut s, "gone"), None);
        assert!(!s.order_status.contains_key("gone"));
        s.configs[0].expires_at = chrono::Utc::now() + chrono::Duration::hours(1);
        assert!(claim(&mut s, "a").is_some(), "a live leg still places");
    }

    /// While the market feed is down (books cleared, bulk cancel in progress),
    /// nothing is placed — not even from a decision made just before the drop.
    #[tokio::test]
    async fn nothing_is_placed_while_the_feed_is_down() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let mut s = e.state.write().await;
        s.feed_down = true;
        assert_eq!(begin_place(&mut s, "a", dec!(0.2), Instant::now()), None);
        assert_eq!(s.order_status.get("a"), None, "untouched");
        s.feed_down = false;
        // Shutting down: refused too, so nothing lands after the final cancel.
        s.stopping = true;
        assert_eq!(begin_place(&mut s, "a", dec!(0.2), Instant::now()), None);
        s.stopping = false;
        assert!(begin_place(&mut s, "a", dec!(0.2), Instant::now()).is_some());
    }

    #[tokio::test]
    async fn begin_place_respects_heartbeat_and_status() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let mut s = e.state.write().await;
        s.heartbeat_paused = true;
        assert!(begin_place(&mut s, "a", dec!(0.21), Instant::now()).is_none());
        s.heartbeat_paused = false;
        assert!(begin_place(&mut s, "a", dec!(0.21), Instant::now()).is_some());
        assert!(begin_place(&mut s, "a", dec!(0.21), Instant::now()).is_none(), "already placing");
    }

    #[tokio::test]
    async fn apply_tick_change_is_a_no_op_for_other_tokens_and_equal_ticks() {
        let e = env(vec![leg("a", dec!(0.01))]);
        assert!(apply_tick_change(&e.state, &e.alerter, "123", dec!(0.001)).await.is_empty());
        assert!(apply_tick_change(&e.state, &e.alerter, TOKEN, dec!(0.01)).await.is_empty());
        let changed = apply_tick_change(&e.state, &e.alerter, TOKEN, dec!(0.001)).await;
        assert_eq!(changed, vec!["leg a (1¢ → 0.1¢)".to_string()]);
    }

    // ── races and failure paths the review asked for ─────────────────────

    #[tokio::test]
    async fn a_late_failure_does_not_touch_a_newer_attempt() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.script(Err("upstream error 500"));
        let old = begin_place(&mut *e.state.write().await, "a", dec!(0.21), Instant::now()).unwrap();
        // A reconnect resets the leg and a new attempt claims it.
        e.state.write().await.order_status.insert("a".into(), OrderStatus::Idle);
        let new = begin_place(&mut *e.state.write().await, "a", dec!(0.21), Instant::now()).unwrap();
        assert_ne!(old, new);
        assert_eq!(run(&e, &api, "a", old, dec!(0.21), dec!(0.01)).await, Outcome::Superseded);
        let s = e.state.read().await;
        assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Placing { price: dec!(0.21) }), "newer claim kept");
        assert_eq!(s.place_attempt.get("a"), Some(&new));
        assert!(s.place_failures.is_empty() && s.place_retry_at.is_empty(), "no failure charged to it");
    }

    #[tokio::test]
    async fn a_late_success_is_cancelled_as_an_orphan() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        let old = begin_place(&mut *e.state.write().await, "a", dec!(0.21), Instant::now()).unwrap();
        e.state.write().await.order_status.insert("a".into(), OrderStatus::Idle);
        let _new = begin_place(&mut *e.state.write().await, "a", dec!(0.21), Instant::now()).unwrap();
        assert_eq!(run(&e, &api, "a", old, dec!(0.21), dec!(0.01)).await, Outcome::Superseded);
        assert_eq!(*api.cancelled.lock().unwrap(), vec!["0xorder".to_string()], "the stray order is cancelled");
        assert!(matches!(e.state.read().await.order_status.get("a"), Some(OrderStatus::Placing { .. })));
    }

    /// The tick couldn't be read because the network failed: nothing is placed
    /// on an unknown tick, and it's an outage (retried), not a rejection.
    #[tokio::test]
    async fn an_unreadable_tick_is_an_outage_not_a_failure() {
        let e = env(vec![leg("a", dec!(0.001))]);
        let api = Fake::new(dec!(0.01), dec!(0.001));
        *api.refresh_fails.lock().unwrap() = true;
        assert_eq!(attempt(&e, &api, "a", dec!(0.213), Instant::now()).await, Some(Outcome::Outage { tries: 1 }));
        assert!(e.state.read().await.place_failures.is_empty(), "not counted");
        assert!(api.placed.lock().unwrap().is_empty(), "nothing placed on an unknown tick");
        assert_eq!(e.state.read().await.configs[0].tick_size, dec!(0.001), "config untouched");
    }

    #[tokio::test]
    async fn a_tick_error_with_an_unchanged_tick_counts_as_a_failure() {
        // The exchange says "tick size" but the tick really is what we use:
        // treat it as a real failure so a broken leg cannot loop forever.
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.script(Err("invalid tick size for this market"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Failed { failures: 1 }));
        assert_eq!(*api.refreshes.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn the_replace_check_requotes_before_cancelling_when_the_price_no_longer_fits() {
        // Used by the replace path before it cancels the live order.
        let e = env(vec![leg("a", dec!(0.001))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        let out = check_tick(&*api, &e.state, &e.alerter, TOKEN, dec!(0.001), dec!(0.213)).await;
        assert_eq!(out, TickCheck::Requote);
        assert_eq!(e.state.read().await.configs[0].tick_size, dec!(0.01), "new tick adopted");
        let ok = check_tick(&*api, &e.state, &e.alerter, TOKEN, dec!(0.01), dec!(0.21)).await;
        assert_eq!(ok, TickCheck::Ok);
        assert_eq!(*api.refreshes.lock().unwrap(), 1, "no request once they agree");
    }

    /// If saving the pause fails, the leg is still paused now and the user is
    /// told it would resume after a restart.
    #[tokio::test(start_paused = true)]
    async fn a_pause_after_rejections_that_cannot_be_saved_is_reported() {
        let e = env(vec![leg("a", dec!(0.01))]);
        // Make the save fail: markets.json becomes a directory.
        let file = e.dir.join("markets.json");
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        let api = Fake::new(dec!(0.01), dec!(0.01));
        for _ in 0..3 {
            api.script(Err("Status: error(400 Bad Request) making POST call to /order with nope"));
        }
        for n in 1..=2 {
            assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Failed { failures: n }));
            tokio::time::advance(Duration::from_secs(61)).await;
        }
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::PausedAfterRejections));
        let s = e.state.read().await;
        assert!(s.configs.len() == 1 && s.configs[0].paused);
        drop(s);
        assert!(alerts(&e).contains("could not be saved"));
    }

    #[tokio::test]
    async fn saves_never_resurrect_a_removed_leg() {
        // The tick save runs under the state lock, so it always writes the
        // current set of legs.
        let e = env(vec![leg("a", dec!(0.01)), leg("b", dec!(0.01))]);
        e.state.write().await.configs.retain(|c| c.id != "b");
        apply_tick_change(&e.state, &e.alerter, TOKEN, dec!(0.001)).await;
        assert_eq!(saved_ticks(&e), vec![dec!(0.001)], "only the remaining leg is saved");
    }

    // ── cancels are confirmed before a leg is freed ──────────────────────────

    fn cancelling(e_state: &mut AppState, id: &str, order: &str, age_secs: u64) {
        let since = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(age_secs))
            .unwrap_or_else(std::time::Instant::now);
        e_state.order_status.insert(id.into(), OrderStatus::Cancelling { order_id: order.into(), since });
    }

    #[tokio::test]
    async fn a_deactivated_leg_is_freed_only_once_its_cancel_is_confirmed() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        // Network failure: the order may still rest, so the leg stays Cancelling.
        cancelling(&mut *e.state.write().await, "a", "ord1", 0);
        api.verify.lock().unwrap().push_back(Err("connection reset".into()));
        cancel_deactivated(&*api, &e.state, "a", "ord1").await;
        assert!(matches!(e.state.read().await.order_status.get("a"), Some(OrderStatus::Cancelling { .. })));
        // Confirmed: freed.
        cancel_deactivated(&*api, &e.state, "a", "ord1").await;
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));
        // A confirmation for an order the leg no longer tracks changes nothing.
        cancelling(&mut *e.state.write().await, "a", "ord2", 0);
        cancel_deactivated(&*api, &e.state, "a", "ord1").await;
        assert!(matches!(e.state.read().await.order_status.get("a"),
            Some(OrderStatus::Cancelling { order_id, .. }) if order_id == "ord2"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_cancel_times_out_and_keeps_the_leg_held() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        *api.verify_hangs.lock().unwrap() = true;
        cancelling(&mut *e.state.write().await, "a", "ord1", 0);
        cancel_deactivated(&*api, &e.state, "a", "ord1").await; // returns after the 20 s bound
        assert!(matches!(e.state.read().await.order_status.get("a"), Some(OrderStatus::Cancelling { .. })));
        assert!(!confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord1").await);
        assert!(matches!(e.state.read().await.order_status.get("a"), Some(OrderStatus::Cancelling { .. })));
    }

    #[tokio::test]
    async fn only_cancels_older_than_the_retry_period_are_claimed_and_only_once() {
        let e = env(vec![leg("a", dec!(0.01)), leg("b", dec!(0.01))]);
        {
            let mut s = e.state.write().await;
            cancelling(&mut s, "a", "old", 31);
            cancelling(&mut s, "b", "fresh", 5);
            s.order_status.insert("c".into(), OrderStatus::Live { order_id: "live".into(), price: dec!(0.2) });
        }
        let first = claim_stuck_cancels(&mut *e.state.write().await);
        assert_eq!(first, vec![("a".to_string(), "old".to_string())]);
        assert!(claim_stuck_cancels(&mut *e.state.write().await).is_empty(), "claiming restarts the clock");
        assert!(matches!(e.state.read().await.order_status.get("a"), Some(OrderStatus::Cancelling { .. })));
    }

    #[tokio::test]
    async fn a_stuck_cancel_is_retried_alerted_once_after_three_tries_and_freed_when_confirmed() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        cancelling(&mut *e.state.write().await, "a", "ord1", 31);
        for _ in 0..4 {
            api.verify.lock().unwrap().push_back(Err("timeout".into()));
        }
        for try_no in 1..=4u32 {
            assert!(!confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord1").await);
            assert!(matches!(e.state.read().await.order_status.get("a"), Some(OrderStatus::Cancelling { .. })));
            assert_eq!(e.state.read().await.cancel_failures.get("a"), Some(&("ord1".to_string(), try_no)));
            let expected = usize::from(try_no >= CANCEL_ALERT_AFTER);
            assert_eq!(alerts(&e).matches("Can't confirm a cancel").count(), expected, "after try {try_no}");
            // A different alert in between, so the Alerter's repeat filter (which
            // only drops a message identical to the last one) can't hide a second one.
            e.alerter.info(format!("unrelated {try_no}"));
        }
        assert!(alerts(&e).contains("ord1"));
        // Confirmed on the fifth try: freed, count cleared, user told it's resolved.
        assert!(confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord1").await);
        let s = e.state.read().await;
        assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle));
        assert!(s.cancel_failures.is_empty());
        drop(s);
        assert!(alerts(&e).contains("Cancel confirmed · leg a"));
    }

    #[tokio::test]
    async fn a_cancel_resolved_elsewhere_or_for_a_new_order_restarts_the_count() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        cancelling(&mut *e.state.write().await, "a", "ord1", 31);
        api.verify.lock().unwrap().push_back(Err("timeout".into()));
        api.verify.lock().unwrap().push_back(Err("timeout".into()));
        confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord1").await;
        confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord1").await;
        assert_eq!(e.state.read().await.cancel_failures.get("a").map(|f| f.1), Some(2));
        // Now cancelling a different order: its count starts again at 1.
        cancelling(&mut *e.state.write().await, "a", "ord2", 31);
        api.verify.lock().unwrap().push_back(Err("timeout".into()));
        confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord2").await;
        assert_eq!(e.state.read().await.cancel_failures.get("a"), Some(&("ord2".to_string(), 1)));
        // Resolved elsewhere (e.g. a reconnect freed it): a late failure changes nothing.
        e.state.write().await.order_status.insert("a".into(), OrderStatus::Idle);
        api.verify.lock().unwrap().push_back(Err("timeout".into()));
        assert!(!confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord2").await);
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));
        assert_eq!(e.state.read().await.cancel_failures.get("a").map(|f| f.1), Some(1), "not counted");
        assert!(!alerts(&e).contains("Can't confirm"));
    }

    #[tokio::test]
    async fn a_confirmed_retry_also_frees_a_leg_put_back_to_live_with_the_same_order() {
        // A Replace's pre-check can restore Live{ord1} while the timer's retry is
        // cancelling ord1; once the retry confirms, the leg must not keep showing
        // an order that no longer exists.
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        e.state
            .write()
            .await
            .order_status
            .insert("a".into(), OrderStatus::Live { order_id: "ord1".into(), price: dec!(0.2) });
        assert!(confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord1").await);
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));
        // A Live leg with a different order is left alone.
        e.state
            .write()
            .await
            .order_status
            .insert("a".into(), OrderStatus::Live { order_id: "ord2".into(), price: dec!(0.2) });
        assert!(confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord1").await);
        assert!(matches!(e.state.read().await.order_status.get("a"),
            Some(OrderStatus::Live { order_id, .. }) if order_id == "ord2"));
    }

    #[tokio::test]
    async fn during_an_outage_retries_go_on_without_counting_or_alerting() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        {
            let mut s = e.state.write().await;
            cancelling(&mut s, "a", "ord1", 31);
            s.heartbeat_paused = true;
        }
        for _ in 0..5 {
            api.verify.lock().unwrap().push_back(Err("unreachable".into()));
            assert!(!confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord1").await);
        }
        assert!(e.state.read().await.cancel_failures.is_empty());
        assert!(!alerts(&e).contains("Can't confirm"));
        assert_eq!(api.cancelled.lock().unwrap().len(), 5, "still retried every time");
        // Back online: the next confirmation frees it.
        e.state.write().await.heartbeat_paused = false;
        assert!(confirm_stuck_cancel(&*api, &e.state, &e.alerter, "a", "ord1").await);
        assert_eq!(e.state.read().await.order_status.get("a"), Some(&OrderStatus::Idle));
    }

    #[tokio::test]
    async fn an_orphan_is_cancelled_and_an_unconfirmed_one_is_reported() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        cancel_orphan(&*api, &e.alerter, "orph1").await;
        assert_eq!(*api.cancelled.lock().unwrap(), vec!["orph1".to_string()]);
        assert!(!alerts(&e).contains("could not be confirmed"));
        api.verify.lock().unwrap().push_back(Ok(false));
        cancel_orphan(&*api, &e.alerter, "orph2").await;
        assert!(alerts(&e).contains("(orph2) could not be confirmed cancelled"));
    }

    #[tokio::test]
    async fn auto_pause_saves_under_the_lock_and_never_brings_back_a_removed_leg() {
        let e = env(vec![leg("a", dec!(0.01)), leg("b", dec!(0.01))]);
        let (label, saved) = auto_pause(&e.state, "a").await.unwrap();
        assert_eq!(label, "leg a");
        assert!(saved.is_ok());
        assert!(e.state.read().await.configs.iter().find(|c| c.id == "a").unwrap().paused);
        let on_disk = crate::storage::load_markets(&e.state.read().await.markets_file).unwrap();
        assert!(on_disk.iter().find(|c| c.id == "a").unwrap().paused, "the pause is saved");
        // "b" is removed (and saved) while "a" is being auto-paused: the auto-pause
        // save always writes the current list, so "b" stays gone.
        {
            let mut s = e.state.write().await;
            s.configs.retain(|c| c.id != "b");
            save_markets(&s.markets_file, &s.configs).unwrap();
        }
        auto_pause(&e.state, "a").await.unwrap().1.unwrap();
        let on_disk = crate::storage::load_markets(&e.state.read().await.markets_file).unwrap();
        assert_eq!(on_disk.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["a"]);
        assert!(auto_pause(&e.state, "b").await.is_none(), "a removed leg is not recreated");
        let on_disk = crate::storage::load_markets(&e.state.read().await.markets_file).unwrap();
        assert_eq!(on_disk.len(), 1);
    }

    #[test]
    fn alignment() {
        assert!(aligned(dec!(0.210), dec!(0.01)));
        assert!(aligned(dec!(0.213), dec!(0.001)));
        assert!(!aligned(dec!(0.213), dec!(0.01)));
        assert!(!aligned(dec!(0.2), dec!(0)), "zero tick never aligns");
    }

    #[test]
    fn tick_errors_are_recognised() {
        assert!(mentions_tick("order 0.213 is invalid: breaks minimum tick size rule: 0.01"));
        assert!(mentions_tick("Invalid TICK_SIZE"));
        assert!(!mentions_tick("upstream error 500"));
    }
}
