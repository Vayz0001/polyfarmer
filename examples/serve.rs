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
use polyfarmer::creds::CredentialStore;
use polyfarmer::engine::orderbook::TokenBook;
use polyfarmer::engine::ws_manager::AppState;
use polyfarmer::rewards::{gamma_resolve, market_data, markets_browse};
use polyfarmer::storage::{append_alert, save_reward_history};
use polyfarmer::types::{Alert, AlertLevel, EnginePhase, MarketConfig, OrderStatus, RewardHistoryFile, RewardSnapshot};
use polyfarmer::web::{events, router, WebState};
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
    let state = WebState::with_config(
        Arc::new(store),
        Arc::clone(&engine),
        None,
        format!("{dir}/reward_history.json").into(),
        alert_tx,
        format!("{dir}/alerts.json").into(),
        Arc::new(tokio::sync::Notify::new()),
    );

    if demo {
        seed_demo(&state, dir).await;
    } else if !state.store.is_initialized() {
        println!("\n  first run — open the dashboard to create your admin password\n");
    }
    events::spawn_state_watcher(state.clone());
    polyfarmer::web::prewarm_browse();

    let bind = "127.0.0.1:8080";
    let listener = tokio::net::TcpListener::bind(bind).await.expect("bind");
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
    let wallet = std::env::var("DEMO_WALLET").ok();
    state.set_wallet_address(wallet);
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

    // Sample history + alerts (only once per demo dir).
    let hist_path = std::path::PathBuf::from(format!("{dir}/reward_history.json"));
    if !hist_path.exists() {
        let today = Utc::now().date_naive();
        let snapshots = (1..=30)
            .map(|d| RewardSnapshot {
                date: today - Duration::days(d),
                total_earnings: Decimal::from((d * 37) % 23 + 4) / dec!(3) + dec!(1.25),
                captured_at: Utc::now(),
            })
            .collect();
        let _ = save_reward_history(&hist_path, &RewardHistoryFile { snapshots });
    }
    let alerts_path = std::path::PathBuf::from(format!("{dir}/alerts.json"));
    if !alerts_path.exists() {
        let label = state.engine.read().await.configs.first().map(|c| c.label.clone()).unwrap_or_else(|| "Demo market".into());
        let mk = |mins: i64, level: AlertLevel, msg: String| Alert { ts: Utc::now() - Duration::minutes(mins), level, message: msg };
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
