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
    /// replies: a slow Polymarket, held for as long as a test needs.
    pub(crate) verify_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Open orders per token, as `open_orders` reports them.
    pub(crate) open: Mutex<std::collections::HashMap<String, Vec<String>>>,
    /// Makes `open_orders` fail (network).
    pub(crate) list_fails: Mutex<bool>,
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
            open: Mutex::new(std::collections::HashMap::new()),
            list_fails: Mutex::new(false),
            bulk_cancelled: Mutex::new(Vec::new()),
        })
    }
    pub(crate) fn script(&self, r: std::result::Result<(), &str>) {
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
    fn place_buy(&self, _t: &str, price: Decimal, shares: Decimal) -> impl Future<Output = Result<String>> + Send {
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
            self.placed_shares.lock().unwrap().push(shares);
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
        let gate = self.verify_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        match self.verify.lock().unwrap().pop_front() {
            Some(Ok(b)) => Ok(b),
            Some(Err(e)) => Err(eyre::eyre!(e)),
            None => Ok(true),
        }
    }
    async fn open_orders(&self, token_id: &str) -> Result<Vec<String>> {
        if *self.list_fails.lock().unwrap() {
            return Err(eyre::eyre!("Internal: error sending request"));
        }
        Ok(self.open.lock().unwrap().get(token_id).cloned().unwrap_or_default())
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
