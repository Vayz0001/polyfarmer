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
//! * **Backoff.** A real failure delays the next attempt (2 s, 4 s, … up to a
//!   minute) instead of retrying on the next book update, so a burst of book
//!   events cannot use up the failure budget in milliseconds.
//! * **Auto-remove.** After `MAX_PLACE_FAILURES` consecutive real failures the
//!   leg is removed, as before.
//!
//! It also holds the cancel and auto-pause steps both callers share: a leg is
//! freed (back to Idle, so it may quote again) only once Polymarket confirms
//! its old order is gone, and an auto-pause is saved under the state lock.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::engine::ws_manager::{AppState, MAX_PLACE_FAILURES};
use crate::storage::save_markets;
use crate::types::OrderStatus;

/// First retry delay after a failed placement; doubles per failure.
const RETRY_BASE: Duration = Duration::from_secs(2);
/// Longest delay between attempts.
const RETRY_MAX: Duration = Duration::from_secs(60);

/// The exchange calls placement needs. `Executor` is the real one; tests use a
/// fake so the whole flow (tick refresh, backoff, auto-remove) runs offline.
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
/// (The dashboard's own saves still copy the list under the lock and write it
/// after; making those save under the lock too is a separate change.)
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
    if s.heartbeat_paused {
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
    /// Collateral still settling: try again on the next evaluation, no failure.
    Settling,
    Failed {
        failures: u32,
    },
    AutoRemoved,
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
        TickCheck::Error(msg) => return fail(&state, &alerter, &config_id, attempt, msg).await,
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
                if !matches!(api.cancel(&order_id).await, Ok(true)) {
                    error!("Could not confirm cancel of orphan order {order_id}");
                    alerter.error(format!(
                        "An order placed while the market was being changed ({order_id}) could not be confirmed \
                         cancelled. Check your open orders on Polymarket."
                    ));
                }
                return Outcome::Superseded;
            }
            s.order_status.insert(config_id.clone(), OrderStatus::Live { order_id, price });
            s.place_attempt.remove(&config_id);
            s.place_failures.remove(&config_id);
            s.place_retry_at.remove(&config_id);
            let (label, token_label) = names(&s, &config_id);
            drop(s);
            alerter.info(format!(
                "Order placed · {label}\nBUY {shares} {token_label} shares @ {}¢ (${order_size})",
                price * Decimal::ONE_HUNDRED
            ));
            Outcome::Placed
        }
        Err(e) => {
            let msg = e.to_string();
            // Collateral locked by a matching/settling order: not the config's fault.
            if msg.contains("not enough balance") {
                warn!("Place skipped: insufficient balance (order settling?) — will retry on next tick");
                let mut s = state.write().await;
                return if release(&mut s, &config_id, attempt) { Outcome::Settling } else { Outcome::Superseded };
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
            fail(&state, &alerter, &config_id, attempt, msg).await
        }
    }
}

/// Count a real failure: back off, and auto-remove the leg at the limit.
async fn fail(state: &Arc<RwLock<AppState>>, alerter: &Alerter, config_id: &str, attempt: u64, msg: String) -> Outcome {
    error!("Place order failed: {msg}");
    alerter.error(format!("Order placement failed: {msg}"));
    let (failures, remove) = {
        let mut s = state.write().await;
        if !release(&mut s, config_id, attempt) {
            return Outcome::Superseded;
        }
        let failures = {
            let f = s.place_failures.entry(config_id.to_string()).or_insert(0);
            *f += 1;
            *f
        };
        s.place_retry_at.insert(config_id.to_string(), Instant::now() + retry_delay(failures));
        if failures < MAX_PLACE_FAILURES {
            (failures, None)
        } else {
            let (label, _) = names(&s, config_id);
            let remaining: Vec<_> = s.configs.iter().filter(|c| c.id != config_id).cloned().collect();
            (failures, Some((label, remaining, s.markets_file.clone())))
        }
    };
    let Some((label, remaining, file)) = remove else {
        return Outcome::Failed { failures };
    };
    match save_markets(&file, &remaining) {
        Ok(()) => {
            let mut s = state.write().await;
            s.configs.retain(|c| c.id != config_id);
            s.order_status.remove(config_id);
            s.place_failures.remove(config_id);
            s.place_retry_at.remove(config_id);
            s.place_attempt.remove(config_id);
            s.cancel_failures.remove(config_id);
            drop(s);
            alerter.error(format!(
                "Market '{label}' auto-removed after {MAX_PLACE_FAILURES} consecutive placement failures"
            ));
            Outcome::AutoRemoved
        }
        Err(e) => {
            error!("Failed to save markets after auto-remove of '{label}': {e}");
            alerter.error(format!(
                "Market '{label}' hit {MAX_PLACE_FAILURES} failures but save failed — will retry on next failure: {e}"
            ));
            Outcome::Failed { failures }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MarketConfig;
    use chrono::Utc;
    use rust_decimal_macros::dec;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    const TOKEN: &str = "20006732765674855733524007935991362439352594042415161039767275461950712817548";

    /// A fake exchange. `sdk` is the order library's cached tick, `real` the
    /// tick Polymarket uses now. Like the real SDK it rejects a price with more
    /// decimal places than its cached tick; like the exchange it rejects a
    /// price that is not a whole number of real ticks.
    struct Fake {
        sdk: Mutex<Decimal>,
        real: Mutex<Decimal>,
        refreshes: Mutex<u32>,
        placed: Mutex<Vec<Decimal>>,
        scripted: Mutex<VecDeque<std::result::Result<(), String>>>,
        refresh_fails: Mutex<bool>,
        cancelled: Mutex<Vec<String>>,
        /// Scripted `cancel_verified` replies (default: confirmed gone).
        verify: Mutex<VecDeque<std::result::Result<bool, String>>>,
        /// Makes `cancel_verified` hang (to exercise the timeout).
        verify_hangs: Mutex<bool>,
    }

    impl Fake {
        fn new(sdk: Decimal, real: Decimal) -> Arc<Self> {
            Arc::new(Self {
                sdk: Mutex::new(sdk),
                real: Mutex::new(real),
                refreshes: Mutex::new(0),
                placed: Mutex::new(Vec::new()),
                scripted: Mutex::new(VecDeque::new()),
                refresh_fails: Mutex::new(false),
                cancelled: Mutex::new(Vec::new()),
                verify: Mutex::new(VecDeque::new()),
                verify_hangs: Mutex::new(false),
            })
        }
        fn script(&self, r: std::result::Result<(), &str>) {
            self.scripted.lock().unwrap().push_back(r.map_err(str::to_string));
        }
    }

    impl OrderApi for Fake {
        fn sdk_tick(&self, _t: &str) -> impl Future<Output = Result<Decimal>> + Send {
            let v = *self.sdk.lock().unwrap();
            async move { Ok(v) }
        }
        fn refresh_tick(&self, _t: &str) -> impl Future<Output = Result<Decimal>> + Send {
            let fails = *self.refresh_fails.lock().unwrap();
            let real = *self.real.lock().unwrap();
            if fails {
                return futures_util::future::Either::Left(async { Err(eyre::eyre!("connection reset")) });
            }
            *self.sdk.lock().unwrap() = real;
            *self.refreshes.lock().unwrap() += 1;
            futures_util::future::Either::Right(async move { Ok(real) })
        }
        fn place_buy(&self, _t: &str, price: Decimal, _shares: Decimal) -> impl Future<Output = Result<String>> + Send {
            let sdk = *self.sdk.lock().unwrap();
            let real = *self.real.lock().unwrap();
            let scripted = self.scripted.lock().unwrap().pop_front();
            let out = if price.normalize().scale() > sdk.scale() {
                Err(eyre::eyre!(
                    "Validation: invalid: Unable to build Order: Price {price} has {} decimal places. Minimum tick size {sdk} has {} decimal places.",
                    price.scale(),
                    sdk.scale()
                ))
            } else if !aligned(price, real) {
                Err(eyre::eyre!("order {price} is invalid: breaks minimum tick size rule: {real}"))
            } else if let Some(Err(e)) = scripted {
                Err(eyre::eyre!(e))
            } else {
                self.placed.lock().unwrap().push(price);
                Ok("0xorder".to_string())
            };
            async move { out }
        }
        async fn cancel(&self, o: &str) -> Result<bool> {
            self.cancelled.lock().unwrap().push(o.to_string());
            Ok(true)
        }
        async fn cancel_verified(&self, o: &str) -> Result<bool> {
            self.cancelled.lock().unwrap().push(o.to_string());
            if *self.verify_hangs.lock().unwrap() {
                std::future::pending::<()>().await;
            }
            match self.verify.lock().unwrap().pop_front() {
                Some(Ok(b)) => Ok(b),
                Some(Err(e)) => Err(eyre::eyre!(e)),
                None => Ok(true),
            }
        }
    }

    fn leg(id: &str, tick: Decimal) -> MarketConfig {
        MarketConfig {
            id: id.to_string(),
            url: "https://polymarket.com/event/x".to_string(),
            label: format!("leg {id}"),
            condition_id: "0xabc".to_string(),
            token_id: TOKEN.to_string(),
            token_label: "Yes".to_string(),
            tick_size: tick,
            distance: dec!(0.023),
            min_depth_between: dec!(500),
            order_size: dec!(150),
            expires_at: Utc::now() + chrono::Duration::days(1),
            paused: false,
            benchmark_bid: None,
            max_volatility: None,
        }
    }

    struct Env {
        state: Arc<RwLock<AppState>>,
        alerter: Arc<Alerter>,
        dir: PathBuf,
    }
    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn env(legs: Vec<MarketConfig>) -> Env {
        let dir = std::env::temp_dir().join(format!("pf-placer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let markets = dir.join("markets.json");
        save_markets(&markets, &legs).unwrap();
        let mut s = AppState::new(markets);
        s.configs = legs;
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        Env { state: Arc::new(RwLock::new(s)), alerter: Arc::new(Alerter::new(&dir.join("alerts.json"), tx)), dir }
    }

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

    // ── part 3: backoff and auto-remove ──────────────────────────────────

    #[test]
    fn retry_delay_doubles_and_is_capped() {
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(2), Duration::from_secs(4));
        assert_eq!(retry_delay(3), Duration::from_secs(8));
        assert_eq!(retry_delay(10), Duration::from_secs(60));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(60), "no overflow");
        assert_eq!(retry_delay(0), Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn failures_back_off_and_the_leg_is_removed_only_after_three_spaced_attempts() {
        use tokio::time::advance;
        let e = env(vec![leg("a", dec!(0.01)), leg("keep", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        for _ in 0..3 {
            api.script(Err("upstream error 500"));
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
        assert_eq!(try_now().await, Some(Outcome::AutoRemoved));
        let s = e.state.read().await;
        assert_eq!(s.configs.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["keep"]);
        assert!(!s.order_status.contains_key("a") && !s.place_failures.contains_key("a"));
        assert!(!s.place_retry_at.contains_key("a"), "no leftover entries");
        drop(s);
        assert_eq!(saved_ticks(&e).len(), 1, "removal saved");
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_clears_the_failure_count_and_the_delay() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.script(Err("timeout"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Failed { failures: 1 }));
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Placed));
        let s = e.state.read().await;
        assert!(s.place_failures.is_empty() && s.place_retry_at.is_empty());
    }

    #[tokio::test]
    async fn settling_collateral_is_not_a_failure() {
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        api.script(Err("not enough balance / allowance"));
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Settling));
        let s = e.state.read().await;
        assert!(s.place_failures.is_empty() && s.place_retry_at.is_empty());
        assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle));
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

    #[tokio::test]
    async fn an_unreadable_tick_counts_as_a_failure() {
        let e = env(vec![leg("a", dec!(0.001))]);
        let api = Fake::new(dec!(0.01), dec!(0.001));
        *api.refresh_fails.lock().unwrap() = true;
        assert_eq!(attempt(&e, &api, "a", dec!(0.213), Instant::now()).await, Some(Outcome::Failed { failures: 1 }));
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

    #[tokio::test(start_paused = true)]
    async fn auto_remove_keeps_the_leg_if_the_save_fails() {
        let e = env(vec![leg("a", dec!(0.01))]);
        // Make the save fail: markets.json becomes a directory.
        let file = e.dir.join("markets.json");
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        let api = Fake::new(dec!(0.01), dec!(0.01));
        for _ in 0..3 {
            api.script(Err("upstream error 500"));
        }
        for n in 1..=2 {
            assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Failed { failures: n }));
            tokio::time::advance(Duration::from_secs(61)).await;
        }
        assert_eq!(attempt(&e, &api, "a", dec!(0.21), Instant::now()).await, Some(Outcome::Failed { failures: 3 }));
        assert_eq!(e.state.read().await.configs.len(), 1, "memory and disk stay in step");
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
    fn alerts(e: &Env) -> String {
        std::fs::read_to_string(e.dir.join("alerts.json")).unwrap_or_default()
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
