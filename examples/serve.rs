//! Preview the dashboard without booting the trading engine (no wallet needed).
//!
//!   cargo run --example serve
//!   → open http://127.0.0.1:8080  (first run asks you to create a password)
//!
//!   DEMO=1 cargo run --example serve
//!   → a populated dashboard for UI work: real reward-eligible markets with
//!     their live books, fake "live" orders, sample alerts and 30 days of
//!     reward history. Nothing is traded. Uses `data-demo/` (password: demo-password).
//!     Optional: DEMO_WALLET=0x… to show that wallet's public positions/fills.

use std::sync::Arc;
use std::time::Instant;

use chrono::{Duration, Utc};
use polyfarmer::cache::TtlCache;
use polyfarmer::creds::CredentialStore;
use polyfarmer::engine::orderbook::TokenBook;
use polyfarmer::engine::ws_manager::AppState;
use polyfarmer::rewards::lp_rank::LpRank;
use polyfarmer::rewards::{gamma_resolve, market_data, markets_browse};
use polyfarmer::storage::append_alert;
use polyfarmer::types::{Alert, AlertLevel, EnginePhase, MarketConfig, OrderStatus};
use polyfarmer::web::{events, router, Caches, WebState};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() {
    let demo = std::env::var("DEMO").is_ok_and(|v| v == "1");
    let dir = if demo { "data-demo" } else { "data" };
    let store = CredentialStore::open(dir).expect("open credential store");

    let engine = Arc::new(RwLock::new(AppState::new(format!("{dir}/markets.json").into())));
    let (alert_tx, _) = tokio::sync::broadcast::channel(64);
    let mut state = WebState::with_config(
        Arc::new(store),
        Arc::clone(&engine),
        None,
        alert_tx,
        format!("{dir}/alerts.json").into(),
        Arc::new(tokio::sync::Notify::new()),
    );
    if demo && std::env::var("DEMO_WALLET").is_err() {
        // Sample reward data never expires, so it isn't replaced by real (empty) reads.
        let forever = std::time::Duration::from_secs(10 * 365 * 86_400);
        state.caches = Arc::new(Caches {
            reward_payouts: Arc::new(TtlCache::new(forever)),
            reward_markets: Arc::new(TtlCache::new(forever)),
            lp_rank: Arc::new(TtlCache::new(forever)),
            ..Caches::default()
        });
    }

    if demo {
        seed_demo(&state, dir).await;
    } else if !state.store.is_initialized() {
        if let Some(code) = state.store.setup_code() {
            println!("\n  first run — setup code: {code}");
            println!("  open http://127.0.0.1:8080/welcome?code={code}\n");
        }
    }
    events::spawn_state_watcher(state.clone());
    polyfarmer::web::prewarm_browse();

    let bind = std::env::var("DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!("dashboard preview → http://{bind}");
    let svc = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, svc).await.expect("serve");
}

async fn seed_demo(state: &WebState, dir: &str) {
    if !state.store.is_initialized() {
        state.store.set_password("demo-password").expect("set password");
    }
    if !state.store.has_wallet() {
        // A throwaway key, never used to trade — only so pages render as configured.
        let key = format!("0x{}", hex::encode(rand::random::<[u8; 32]>()));
        state.store.set_wallet(&key, "0x0000000000000000000000000000000000000001").expect("set wallet");
    }
    // DEMO_WALLET=<address> shows that wallet's real public payouts and rank;
    // without it the Rewards page gets sample data.
    let wallet = std::env::var("DEMO_WALLET").ok();
    let sample_rewards = wallet.is_none();
    state.set_wallet_address(Some(wallet.unwrap_or_else(|| DEMO_ADDRESS.into())));
    println!("\n  DEMO mode — password: demo-password\n");

    // Top reward markets by daily pool → one both-sides and two one-side farms.
    let query = markets_browse::BrowseQuery {
        order_by: Some("rate_per_day".into()),
        position: Some("DESC".into()),
        page_size: Some(12),
        ..Default::default()
    };
    let picks = match markets_browse::browse(&query).await {
        Ok(r) => r.data,
        Err(e) => {
            eprintln!("demo: could not reach Polymarket ({e}) — starting with no markets");
            Vec::new()
        }
    };

    let mut configs: Vec<MarketConfig> = Vec::new();
    let mut statuses = Vec::new();
    let mut books = Vec::new();
    for (n, m) in picks.iter().filter(|m| m.spread < dec!(0.05)).take(4).enumerate() {
        let Ok(mr) = gamma_resolve::market_by_slug(&m.market_slug).await else { continue };
        let legs: Vec<usize> = if n == 0 { vec![0, 1] } else { vec![0] };
        for side in legs {
            let token = mr.token_ids[side].clone();
            let Ok(book) = market_data::fetch_book(&token).await else { continue };
            let Some(bb) = book.best_bid else { continue };
            let distance = (mr.tick_size * dec!(2)).max(dec!(0.01));
            let id = format!("mar_demo_{n}{side}");
            configs.push(MarketConfig {
                id: id.clone(),
                url: format!("https://polymarket.com/event/{}/{}", m.event_slug, m.market_slug),
                label: mr.question.clone(),
                condition_id: mr.condition_id.clone(),
                token_id: token.clone(),
                token_label: mr.outcomes[side].clone(),
                tick_size: mr.tick_size,
                distance,
                min_depth_between: dec!(250),
                order_size: if n == 0 { dec!(150) } else { dec!(100) },
                expires_at: Utc::now() + Duration::days(if n == 2 { 36500 } else { 6 }),
                paused: n == 3,
                benchmark_bid: Some(bb),
                max_volatility: if n == 1 { Some(dec!(0.05)) } else { None },
            });
            let status = match n {
                3 => OrderStatus::Idle,
                2 => OrderStatus::Placing { price: bb - distance },
                _ => OrderStatus::Live { order_id: format!("0xdemo{n}{side}"), price: bb - distance },
            };
            statuses.push((id, status));
            books.push((token, to_token_book(&book)));
        }
    }

    {
        let mut s = state.engine.write().await;
        s.configs = configs;
        for (id, st) in statuses {
            s.order_status.insert(id, st);
        }
        for (t, b) in books {
            s.books.insert(t, b);
        }
        s.engine_phase = EnginePhase::Running;
        s.ws_connected = true;
        s.last_ws_msg = Some(Instant::now());
        s.last_heartbeat_ok = Some(Instant::now());
    }

    if sample_rewards {
        seed_sample_rewards(state).await;
    }

    // Sample alerts (only once per demo dir).
    let alerts_path = std::path::PathBuf::from(format!("{dir}/alerts.json"));
    if !alerts_path.exists() {
        let label =
            state.engine.read().await.configs.first().map(|c| c.label.clone()).unwrap_or_else(|| "Demo market".into());
        let mk = |mins: i64, level: AlertLevel, msg: String| Alert {
            ts: Utc::now() - Duration::minutes(mins),
            level,
            message: msg,
        };
        for a in [
            mk(400, AlertLevel::Info, "Bot started".into()),
            mk(399, AlertLevel::Info, "WS connected".into()),
            mk(398, AlertLevel::Info, format!("Order placed · {label}\nBUY 166.67 Yes shares @ 45¢ ($75)")),
            mk(220, AlertLevel::Info, format!("Order replaced · {label}\nBUY 163.04 Yes shares @ 46¢ ($75)")),
            mk(130, AlertLevel::Warn, "WS disconnected (stream ended) — cancelling 3 open orders".into()),
            mk(129, AlertLevel::Info, "WS connected".into()),
            mk(64, AlertLevel::Info, format!("Order cancelled · {label}\n$75 of Yes — depth ahead fell below your minimum")),
            mk(30, AlertLevel::Warn, format!("Auto-paused · {label}\nBest bid moved past your volatility limit — resume from Markets when stable")),
            mk(12, AlertLevel::Error, "Order placement failed: not enough balance / allowance".into()),
            mk(3, AlertLevel::Info, "Hourly summary: 3 active markets, 3 live orders".into()),
        ] {
            let _ = append_alert(&alerts_path, &a);
        }
    }

    // Keep the demo "alive": fresh heartbeats + real books every few seconds.
    let engine = Arc::clone(&state.engine);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let tokens: Vec<String> = engine.read().await.books.keys().cloned().collect();
            let mut fresh = Vec::new();
            for t in tokens {
                if let Ok(b) = market_data::fetch_book(&t).await {
                    fresh.push((t, to_token_book(&b)));
                }
            }
            let mut s = engine.write().await;
            for (t, b) in fresh {
                s.books.insert(t, b);
            }
            s.last_ws_msg = Some(Instant::now());
            s.last_heartbeat_ok = Some(Instant::now());
        }
    });
}

const DEMO_ADDRESS: &str = "0x00000000000000000000000000000000000000de";

/// 30 days of payouts, today's per-market earnings for the demo farms (plus one
/// removed market that paid), and a leaderboard rank.
async fn seed_sample_rewards(state: &WebState) {
    let today = Utc::now().date_naive();
    let payouts: Vec<_> = (1..=30)
        .map(|d: i64| {
            let paid = (today - Duration::days(d - 1)).and_hms_opt(0, 0, 10).unwrap().and_utc();
            let usd = Decimal::from((d * 37) % 23 + 4) / dec!(3) + dec!(1.25);
            serde_json::from_value(serde_json::json!({
                "timestamp": paid.timestamp(), "type": "REWARD", "usdc_size": usd.round_dp(4).to_string(),
            }))
            .unwrap()
        })
        .collect();
    state.caches.reward_payouts.put(DEMO_ADDRESS, payouts);

    let zero = format!("0x{}", "0".repeat(40));
    let entry = |cid: &str, question: &str, slug: &str, pct: &str, earned: &str, pool: &str| {
        serde_json::from_value(serde_json::json!({
            "condition_id": cid, "question": question, "market_slug": slug, "event_slug": slug, "image": "",
            "rewards_max_spread": "3", "rewards_min_size": "100", "market_competitiveness": "1",
            "maker_address": zero, "earning_percentage": pct,
            "earnings": [{ "asset_address": zero, "earnings": earned, "asset_rate": "1" }],
            "rewards_config": [{ "asset_address": zero, "start_date": "2026-01-01", "end_date": "2099-01-01",
                                 "rate_per_day": pool, "total_rewards": "0" }],
        }))
        .unwrap()
    };
    let mut seen = Vec::new();
    let mut entries = Vec::new();
    for c in state.engine.read().await.configs.iter() {
        if seen.contains(&c.condition_id) {
            continue;
        }
        // Per demo market: farming, farming, waiting, paused (after earning earlier).
        let (pct, earned) = [("4.12", "1.8312"), ("2.50", "0.9120"), ("0", "0"), ("0", "0.4400")][seen.len() % 4];
        seen.push(c.condition_id.clone());
        let slug = c.url.rsplit('/').next().unwrap_or_default();
        entries.push(entry(&c.condition_id, &c.label, slug, pct, earned, "250"));
    }
    entries.push(entry(&format!("0x{}", "ab".repeat(32)), "A market you removed today", "", "0", "0.2150", "100"));
    state.caches.reward_markets.put(&today.to_string(), Arc::new(entries));
    state.caches.lp_rank.put(DEMO_ADDRESS, LpRank::Ranked { rank: 1_482, total: 171_709 });
}

fn to_token_book(b: &market_data::BookSnapshot) -> TokenBook {
    let mut tb = TokenBook::default();
    for (p, s) in &b.bids {
        tb.bids.insert(*p, *s);
    }
    for (p, s) in &b.asks {
        tb.asks.insert(*p, *s);
    }
    tb.best_bid = b.best_bid;
    tb.best_ask = b.best_ask;
    tb
}
