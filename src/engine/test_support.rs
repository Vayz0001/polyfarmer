//! Test-only helpers shared by the engine's tests: a fake exchange
//! ([`Fake`], an [`OrderApi`]), a leg builder and a temporary environment
//! (state, alerter and a data folder that is removed afterwards).

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use eyre::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::RwLock;

use crate::engine::alerts::Alerter;
use crate::engine::placer::{aligned, OrderApi};
use crate::engine::ws_manager::AppState;
use crate::storage::save_markets;
use crate::types::MarketConfig;

pub(crate) const TOKEN: &str = "20006732765674855733524007935991362439352594042415161039767275461950712817548";

/// A fake exchange. `sdk` is the order library's cached tick, `real` the
/// tick Polymarket uses now. Like the real SDK it rejects a price with more
/// decimal places than its cached tick; like the exchange it rejects a
/// price that is not a whole number of real ticks.
///
/// Each `*_gate`, when set, holds one step of a call until the test notifies
/// it: `notify_one` lets one held call go on (or, if none is held yet, the
/// next one straight through), `notify_waiters` every call held right now.
pub(crate) struct Fake {
    pub(crate) sdk: Mutex<Decimal>,
    pub(crate) real: Mutex<Decimal>,
    pub(crate) refreshes: Mutex<u32>,
    pub(crate) placed: Mutex<Vec<Decimal>>,
    /// Shares of each placed order, in order.
    pub(crate) placed_shares: Mutex<Vec<Decimal>>,
    pub(crate) scripted: Mutex<VecDeque<std::result::Result<(), String>>>,
    pub(crate) refresh_fails: Mutex<bool>,
    pub(crate) cancelled: Mutex<Vec<String>>,
    /// Scripted `cancel_verified` replies (default: confirmed gone).
    pub(crate) verify: Mutex<VecDeque<std::result::Result<bool, String>>>,
    /// Makes `cancel_verified` hang (to exercise the timeout).
    pub(crate) verify_hangs: Mutex<bool>,
    /// When set, `cancel_verified` waits for this to be notified before it
    /// replies: a slow Polymarket, held for as long as a test needs. With
    /// `keeps_book` the order stays listed until the confirmation goes out.
    pub(crate) verify_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Off by default: every accepted order is "0xorder" and `open` changes
    /// only when a test edits it or `cancel_many` runs. When on, the fake keeps
    /// its book like the exchange: the n-th accepted order (the n-th entry of
    /// `placed`) is "0xorder-n" and is listed in `open` under its token from
    /// the moment it is accepted until a cancel of it is confirmed.
    pub(crate) keeps_book: Mutex<bool>,
    /// When set, `place_buy` waits for this before the exchange takes the
    /// order: while held, the order is neither in `placed` nor listed.
    pub(crate) accept_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// When set, `place_buy` waits for this to be notified before it replies
    /// (the order has reached Polymarket; the answer is slow). An accepted
    /// order is already in `placed` and, with `keeps_book`, listed.
    pub(crate) place_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Open orders per token, as `open_orders` reports them.
    pub(crate) open: Mutex<std::collections::HashMap<String, Vec<String>>>,
    /// Makes `open_orders` fail (network).
    pub(crate) list_fails: Mutex<bool>,
    /// When set, `open_orders` reads the book when it is called and waits for
    /// this before it replies: the reply is the book as it was at the call,
    /// whatever changed while it was held.
    pub(crate) list_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Orders cancelled through `cancel_many`.
    pub(crate) bulk_cancelled: Mutex<Vec<String>>,
}

impl Fake {
    pub(crate) fn new(sdk: Decimal, real: Decimal) -> Arc<Self> {
        Arc::new(Self {
            sdk: Mutex::new(sdk),
            real: Mutex::new(real),
            refreshes: Mutex::new(0),
            placed: Mutex::new(Vec::new()),
            placed_shares: Mutex::new(Vec::new()),
            scripted: Mutex::new(VecDeque::new()),
            refresh_fails: Mutex::new(false),
            cancelled: Mutex::new(Vec::new()),
            verify: Mutex::new(VecDeque::new()),
            verify_hangs: Mutex::new(false),
            verify_gate: Mutex::new(None),
            keeps_book: Mutex::new(false),
            accept_gate: Mutex::new(None),
            place_gate: Mutex::new(None),
            open: Mutex::new(std::collections::HashMap::new()),
            list_fails: Mutex::new(false),
            list_gate: Mutex::new(None),
            bulk_cancelled: Mutex::new(Vec::new()),
        })
    }
    pub(crate) fn script(&self, r: std::result::Result<(), &str>) {
        self.scripted.lock().unwrap().push_back(r.map_err(str::to_string));
    }
    /// The exchange takes or refuses a placement. An accepted order is
    /// recorded in `placed` and, with `keeps_book`, numbered and listed.
    fn take_order(&self, token: &str, price: Decimal, shares: Decimal) -> Result<String> {
        let sdk = *self.sdk.lock().unwrap();
        let real = *self.real.lock().unwrap();
        let scripted = self.scripted.lock().unwrap().pop_front();
        if price.normalize().scale() > sdk.scale() {
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
            let n = {
                let mut placed = self.placed.lock().unwrap();
                placed.push(price);
                placed.len()
            };
            self.placed_shares.lock().unwrap().push(shares);
            if *self.keeps_book.lock().unwrap() {
                let id = format!("0xorder-{n}");
                self.open.lock().unwrap().entry(token.to_string()).or_default().push(id.clone());
                Ok(id)
            } else {
                Ok("0xorder".to_string())
            }
        }
    }
    /// With `keeps_book`, a cancelled order leaves the book.
    fn unlist(&self, order_id: &str) {
        if *self.keeps_book.lock().unwrap() {
            for ids in self.open.lock().unwrap().values_mut() {
                ids.retain(|id| id != order_id);
            }
        }
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
    async fn place_buy(&self, t: &str, price: Decimal, shares: Decimal) -> Result<String> {
        let accept = self.accept_gate.lock().unwrap().clone();
        if let Some(gate) = accept {
            gate.notified().await;
        }
        let out = self.take_order(t, price, shares);
        let gate = self.place_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        out
    }
    async fn cancel(&self, o: &str) -> Result<bool> {
        self.cancelled.lock().unwrap().push(o.to_string());
        self.unlist(o);
        Ok(true)
    }
    async fn cancel_verified(&self, o: &str) -> Result<bool> {
        self.cancelled.lock().unwrap().push(o.to_string());
        if *self.verify_hangs.lock().unwrap() {
            std::future::pending::<()>().await;
        }
        let gate = self.verify_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let reply = self.verify.lock().unwrap().pop_front();
        match reply {
            Some(Ok(false)) => Ok(false),
            Some(Err(e)) => Err(eyre::eyre!(e)),
            Some(Ok(true)) | None => {
                self.unlist(o);
                Ok(true)
            }
        }
    }
    async fn open_orders(&self, token_id: &str) -> Result<Vec<String>> {
        let listing = if *self.list_fails.lock().unwrap() {
            Err(eyre::eyre!("Internal: error sending request"))
        } else {
            Ok(self.open.lock().unwrap().get(token_id).cloned().unwrap_or_default())
        };
        let gate = self.list_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        listing
    }
    async fn cancel_many(&self, order_ids: &[String]) -> Result<Vec<String>> {
        let mut open = self.open.lock().unwrap();
        for ids in open.values_mut() {
            ids.retain(|id| !order_ids.contains(id));
        }
        self.bulk_cancelled.lock().unwrap().extend(order_ids.iter().cloned());
        Ok(Vec::new())
    }
}

pub(crate) fn leg(id: &str, tick: Decimal) -> MarketConfig {
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

pub(crate) struct Env {
    pub(crate) state: Arc<RwLock<AppState>>,
    pub(crate) alerter: Arc<Alerter>,
    pub(crate) dir: PathBuf,
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn env(legs: Vec<MarketConfig>) -> Env {
    let dir = std::env::temp_dir().join(format!("pf-placer-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let markets = dir.join("markets.json");
    save_markets(&markets, &legs).unwrap();
    let mut s = AppState::new(markets);
    s.configs = legs;
    let (tx, _rx) = tokio::sync::broadcast::channel(64);
    Env { state: Arc::new(RwLock::new(s)), alerter: Arc::new(Alerter::new(&dir.join("alerts.json"), tx)), dir }
}
pub(crate) fn alerts(e: &Env) -> String {
    std::fs::read_to_string(e.dir.join("alerts.json")).unwrap_or_default()
}

mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use rust_decimal_macros::dec;
    use tokio::sync::Notify;

    use super::{Fake, TOKEN};
    use crate::engine::placer::OrderApi;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|&s| s.to_string()).collect()
    }

    /// A fake that keeps its book like the exchange.
    fn book() -> Arc<Fake> {
        let api = Fake::new(dec!(0.01), dec!(0.01));
        *api.keeps_book.lock().unwrap() = true;
        api
    }

    /// Lets spawned calls run up to their gate (time is paused: no real wait).
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn by_default_every_order_is_0xorder_and_cancels_leave_the_listing_alone() {
        let api = Fake::new(dec!(0.01), dec!(0.01));
        assert_eq!(api.place_buy(TOKEN, dec!(0.2), dec!(750)).await.unwrap(), "0xorder");
        assert_eq!(api.place_buy(TOKEN, dec!(0.21), dec!(750)).await.unwrap(), "0xorder");
        assert!(api.open_orders(TOKEN).await.unwrap().is_empty(), "nothing listed by itself");
        api.open.lock().unwrap().insert(TOKEN.into(), ids(&["0xorder"]));
        assert!(api.cancel_verified("0xorder").await.unwrap());
        assert!(api.cancel("0xorder").await.unwrap());
        assert_eq!(api.open_orders(TOKEN).await.unwrap(), ids(&["0xorder"]), "only a test edits the listing");
    }

    #[tokio::test]
    async fn keeping_the_book_numbers_each_accepted_order_and_lists_it_until_a_confirmed_cancel() {
        let api = book();
        api.script(Ok(()));
        api.script(Err("invalid order size"));
        assert_eq!(api.place_buy(TOKEN, dec!(0.2), dec!(750)).await.unwrap(), "0xorder-1");
        assert!(api.place_buy(TOKEN, dec!(0.2), dec!(750)).await.is_err(), "a rejection gets no id");
        assert_eq!(api.place_buy("other", dec!(0.3), dec!(500)).await.unwrap(), "0xorder-2");
        assert_eq!(api.place_buy(TOKEN, dec!(0.21), dec!(750)).await.unwrap(), "0xorder-3");
        assert_eq!(api.open_orders(TOKEN).await.unwrap(), ids(&["0xorder-1", "0xorder-3"]));
        assert_eq!(api.open_orders("other").await.unwrap(), ids(&["0xorder-2"]));

        api.verify.lock().unwrap().push_back(Ok(false));
        api.verify.lock().unwrap().push_back(Err("timeout".into()));
        assert!(!api.cancel_verified("0xorder-1").await.unwrap());
        assert!(api.cancel_verified("0xorder-1").await.is_err());
        assert_eq!(api.open_orders(TOKEN).await.unwrap(), ids(&["0xorder-1", "0xorder-3"]), "unconfirmed: listed");
        assert!(api.cancel_verified("0xorder-1").await.unwrap());
        assert!(api.cancel("0xorder-3").await.unwrap());
        assert!(api.open_orders(TOKEN).await.unwrap().is_empty());
        assert_eq!(api.open_orders("other").await.unwrap(), ids(&["0xorder-2"]));
    }

    /// `accept_gate` holds a placement before the exchange takes it,
    /// `place_gate` holds the reply after: two separate steps.
    #[tokio::test(start_paused = true)]
    async fn a_placement_can_be_held_before_and_after_the_exchange_takes_it() {
        let api = book();
        let (accept, reply) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        *api.accept_gate.lock().unwrap() = Some(Arc::clone(&accept));
        *api.place_gate.lock().unwrap() = Some(Arc::clone(&reply));
        let call = tokio::spawn({
            let api = Arc::clone(&api);
            async move { api.place_buy(TOKEN, dec!(0.2), dec!(750)).await }
        });
        settle().await;
        assert!(!call.is_finished());
        assert!(api.placed.lock().unwrap().is_empty());
        assert!(api.open_orders(TOKEN).await.unwrap().is_empty(), "not on the book yet");

        accept.notify_one();
        settle().await;
        assert!(!call.is_finished(), "taken, the reply still held");
        assert_eq!(*api.placed.lock().unwrap(), vec![dec!(0.2)]);
        assert_eq!(api.open_orders(TOKEN).await.unwrap(), ids(&["0xorder-1"]), "on the book already");

        reply.notify_one();
        assert_eq!(call.await.unwrap().unwrap(), "0xorder-1");
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_listing_replies_with_the_book_as_it_was_when_asked() {
        let api = book();
        let list = Arc::new(Notify::new());
        assert_eq!(api.place_buy(TOKEN, dec!(0.2), dec!(750)).await.unwrap(), "0xorder-1");
        *api.list_gate.lock().unwrap() = Some(Arc::clone(&list));
        let listing = tokio::spawn({
            let api = Arc::clone(&api);
            async move { api.open_orders(TOKEN).await }
        });
        settle().await;
        assert!(!listing.is_finished());
        assert_eq!(api.place_buy(TOKEN, dec!(0.21), dec!(750)).await.unwrap(), "0xorder-2", "lands meanwhile");

        list.notify_one();
        assert_eq!(listing.await.unwrap().unwrap(), ids(&["0xorder-1"]), "the book at the call");
        *api.list_gate.lock().unwrap() = None;
        assert_eq!(api.open_orders(TOKEN).await.unwrap(), ids(&["0xorder-1", "0xorder-2"]));
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_cancel_confirmation_keeps_the_order_listed_until_it_goes_out() {
        let api = book();
        let confirm = Arc::new(Notify::new());
        *api.verify_gate.lock().unwrap() = Some(Arc::clone(&confirm));
        assert_eq!(api.place_buy(TOKEN, dec!(0.2), dec!(750)).await.unwrap(), "0xorder-1");
        let cancel = tokio::spawn({
            let api = Arc::clone(&api);
            async move { api.cancel_verified("0xorder-1").await }
        });
        settle().await;
        assert!(!cancel.is_finished());
        assert_eq!(*api.cancelled.lock().unwrap(), ids(&["0xorder-1"]), "sent");
        assert_eq!(api.open_orders(TOKEN).await.unwrap(), ids(&["0xorder-1"]));

        confirm.notify_one();
        assert!(cancel.await.unwrap().unwrap());
        assert!(api.open_orders(TOKEN).await.unwrap().is_empty());
    }
}
