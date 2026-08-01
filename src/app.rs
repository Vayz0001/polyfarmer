//! Application orchestration: boots the engine tasks + the web dashboard,
//! then waits for shutdown and cancels open orders.

use crate::config::Config;
use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::engine::quoter::QuoteAction;
use crate::engine::{heartbeat, quoter, ws_manager};
use crate::engine::ws_manager::{AppState, MAX_PLACE_FAILURES};
use crate::storage::{load_markets, save_markets};
use crate::types::{EnginePhase, OrderStatus, WsCommand};

use eyre::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Notify, RwLock};
use tracing::{error, info, warn};

const QUOTE_TIMER_INTERVAL: Duration = Duration::from_secs(30);
const HOURLY_SUMMARY_INTERVAL: Duration = Duration::from_secs(3600);
/// How often the boot task re-checks for a newly-configured wallet (cheap,
/// local file read — fine to be snappy).
const WALLET_POLL: Duration = Duration::from_secs(3);
/// Backoff between auto-retries after a failed engine start (auth/network).
/// Longer than WALLET_POLL so a sustained outage doesn't hammer Polymarket.
const START_RETRY: Duration = Duration::from_secs(10);

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("polyfarmer=info")),
        )
        .init();

    info!("================================");
    info!("   polyfarmer starting");
    info!("================================");

    let config = Arc::new(Config::from_env()?);

    // ── Credential store ──────────────────────────────────────────────────────
    let store = Arc::new(crate::creds::CredentialStore::open(config.data_dir.clone())?);
    if !store.is_initialized() {
        info!("First run — open the dashboard to create your admin password.");
    }

    // ── Shared engine state ────────────────────────────────────────────────────
    // Created up-front and shared with the dashboard, so the UI can read live
    // markets/orders even before the wallet is configured (engine idle until then).
    let initial_configs = load_markets(&config.markets_file)?;
    info!("Loaded {} market configs", initial_configs.len());
    let state = Arc::new(RwLock::new({
        let mut s = AppState::new(config.markets_file.clone());
        for cfg in &initial_configs {
            s.order_status.insert(cfg.id.clone(), OrderStatus::Idle);
        }
        s.configs = initial_configs;
        s
    }));

    let alerter = Arc::new(Alerter::new(&config.alerts_file));

    // ── Spawn: web dashboard (reachable even before the wallet is configured) ──
    // Returns a handle the boot task awaits, so configuring a wallet in the
    // dashboard starts the engine immediately — no restart on first run.
    let (wallet_ready, engine_handle) = {
        let bind = config.dashboard_bind.clone();
        match tokio::net::TcpListener::bind(&bind).await {
            Ok(listener) => {
                info!("Dashboard on http://{}", bind);
                let web_state = crate::web::WebState::with_config(
                    Arc::clone(&store),
                    Arc::clone(&state),
                    config.polygon_rpc_url.clone(),
                    config.reward_history_file.clone(),
                );
                let ready = Arc::clone(&web_state.wallet_ready);
                let handle = web_state.engine_handle.clone();
                let svc = crate::web::router(web_state)
                    .into_make_service_with_connect_info::<std::net::SocketAddr>();
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, svc).await {
                        error!("Web server error: {}", e);
                    }
                });
                (ready, handle)
            }
            // No dashboard means no way to configure a wallet at runtime; the
            // wait-loop below falls back to its periodic re-check of the file.
            Err(e) => {
                error!("Failed to bind dashboard on {}: {}", bind, e);
                (Arc::new(tokio::sync::Notify::new()), crate::web::EngineHandle::new())
            }
        }
    };

    // ── Wallet credentials: wait until one is configured, then build the
    //    executor. Each leg can fail without killing the process — on failure
    //    we set EnginePhase::Error and loop back to waiting, so re-saving the
    //    wallet in the dashboard retries. This is the first-run auto-start path.
    // Re-checks happen every few seconds; these flags keep us from flooding the
    // log / external notifier while idle-waiting or during a sustained outage.
    let mut waiting_logged = false;
    let mut failure_alerted = false;

    let executor = loop {
        let creds = match store.load_wallet() {
            Ok(Some(c)) => c,
            Ok(None) => {
                set_phase(&state, EnginePhase::AwaitingWallet).await;
                if !waiting_logged {
                    info!("No wallet configured — waiting (configure at http://{})", config.dashboard_bind);
                    waiting_logged = true;
                }
                if wait_or_exit(&wallet_ready, WALLET_POLL).await { return Ok(()); }
                continue;
            }
            Err(e) => {
                // A store read error (e.g. corrupt master.key) is not something
                // the user can fix from the dashboard — fail loudly.
                return Err(eyre::eyre!("Failed to read wallet store: {}", e));
            }
        };

        let proxy_wallet: alloy::primitives::Address = match creds.proxy_wallet.parse() {
            Ok(a) => a,
            Err(_) => {
                error!("Stored proxy wallet is not a valid address — re-enter it in the dashboard");
                if !failure_alerted {
                    alerter.error("Stored wallet address is invalid — re-enter it in the dashboard.");
                    failure_alerted = true;
                }
                set_phase(&state, EnginePhase::Error).await;
                if wait_or_exit(&wallet_ready, START_RETRY).await { return Ok(()); }
                continue;
            }
        };

        set_phase(&state, EnginePhase::Starting).await;

        let executor = match Executor::new(creds.expose_key(), proxy_wallet, config.polygon_rpc_url.as_deref()).await {
            Ok(e) => Arc::new(e),
            Err(e) => {
                error!("Could not authenticate with Polymarket: {}", e);
                if !failure_alerted {
                    alerter.error("Wallet saved, but the engine couldn't start (Polymarket auth/network). It will keep retrying — check logs.");
                    failure_alerted = true;
                }
                set_phase(&state, EnginePhase::Error).await;
                if wait_or_exit(&wallet_ready, START_RETRY).await { return Ok(()); }
                continue;
            }
        };

        // ── Startup safety: cancel any open orders on tracked markets ─────────
        let startup_tokens: Vec<String> = {
            let s = state.read().await;
            s.configs.iter()
                .map(|c| c.token_id.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect()
        };
        if let Err(e) = executor.cancel_orders_for_tokens(&startup_tokens).await {
            error!("Startup cancel failed — refusing to trade with potentially open orders: {}", e);
            if !failure_alerted {
                alerter.error("Engine start aborted: could not clear existing orders. It will keep retrying — check logs.");
                failure_alerted = true;
            }
            set_phase(&state, EnginePhase::Error).await;
            if wait_or_exit(&wallet_ready, START_RETRY).await { return Ok(()); }
            continue;
        }

        break executor;
    };

    set_phase(&state, EnginePhase::Running).await;

    // ── Channels ──────────────────────────────────────────────────────────────
    let (ws_cmd_tx, ws_cmd_rx) = mpsc::channel::<WsCommand>(20);
    let (stop_tx, stop_rx) = watch::channel(false);

    // Make the authenticated executor + WS command sender reachable from web
    // handlers (market add/remove/pause, reward reads) — see web/state.rs.
    engine_handle.set(Arc::clone(&executor), ws_cmd_tx.clone()).await;

    // ── Spawn: WebSocket manager ──────────────────────────────────────────────
    ws_manager::spawn(
        Arc::clone(&state),
        Arc::clone(&executor),
        Arc::clone(&alerter),
        ws_cmd_rx,
        stop_rx.clone(),
    );

    // ── Spawn: Heartbeat ──────────────────────────────────────────────────────
    heartbeat::spawn(
        Arc::clone(&executor),
        Arc::clone(&alerter),
        Arc::clone(&state),
        stop_rx.clone(),
    );

    // ── Spawn: daily reward-history snapshot ──────────────────────────────────
    // markets.json no longer needs a poll loop — web handlers (Segment 5) own
    // the only write path now and mutate `AppState` directly; ws_manager's
    // connect loop always re-derives its subscription set fresh from
    // `AppState.configs` on every (re)connect, so there's nothing left to
    // reconcile from a periodic file diff.
    crate::rewards::history::spawn(
        Arc::clone(&executor),
        Arc::clone(&alerter),
        config.reward_history_file.clone(),
        stop_rx.clone(),
    );

    // ── Spawn: 30s quote timer (fallback re-evaluation) ───────────────────────
    {
        let state2   = Arc::clone(&state);
        let executor2 = Arc::clone(&executor);
        let alerter2  = Arc::clone(&alerter);
        let mut stop  = stop_rx.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(QUOTE_TIMER_INTERVAL);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        evaluate_all_markets(&state2, &executor2, &alerter2).await;
                    }
                    _ = stop.changed() => {
                        if *stop.borrow() { break; }
                    }
                }
            }
        });
    }

    // ── Spawn: hourly summary ─────────────────────────────────────────────────
    {
        let state2   = Arc::clone(&state);
        let alerter2  = Arc::clone(&alerter);
        let mut stop  = stop_rx.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(HOURLY_SUMMARY_INTERVAL);
            ticker.tick().await; // skip first immediate tick
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let s = state2.read().await;
                        let active  = s.configs.iter().filter(|c| !c.paused).count();
                        let live    = s.order_status.values()
                            .filter(|s| matches!(s, OrderStatus::Live { .. }))
                            .count();
                        alerter2.info(format!(
                            "Hourly summary: {} active markets, {} live orders",
                            active, live
                        ));
                    }
                    _ = stop.changed() => {
                        if *stop.borrow() { break; }
                    }
                }
            }
        });
    }

    alerter.info("Bot started");
    info!("All tasks spawned, bot is running");

    // ── Graceful shutdown on SIGINT / SIGTERM ─────────────────────────────────
    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received — cancelling bot orders...");
    alerter.warn("Bot shutting down — cancelling open orders");

    let _ = stop_tx.send(true);

    // Include Cancelling — the in-flight cancel may not have completed before shutdown.
    let live_order_ids: Vec<String> = {
        let s = state.read().await;
        s.order_status.values().filter_map(|status| {
            match status {
                OrderStatus::Live { order_id, .. } |
                OrderStatus::Cancelling { order_id, .. } => Some(order_id.clone()),
                _ => None,
            }
        }).collect()
    };
    if let Err(e) = executor.cancel_orders(&live_order_ids).await {
        error!("Shutdown cancel failed: {}", e);
    }

    info!("Shutdown complete");
    Ok(())
}

// ── Boot helpers ────────────────────────────────────────────────────────────

/// Set the engine phase on shared state (brief write lock, released at once).
async fn set_phase(state: &Arc<RwLock<AppState>>, phase: EnginePhase) {
    state.write().await.engine_phase = phase;
}

/// Park until the wallet is (re)configured, a periodic re-check fires, or the
/// process is asked to shut down. Returns `true` when we should exit now.
///
/// The `notified()` arm is the fast path (fires the instant `set_wallet` pings
/// it); the 3s sleep is belt-and-suspenders so a missed notify still self-heals
/// — the caller re-reads the file each loop, so the bot can't silently hang.
async fn wait_or_exit(wallet_ready: &Notify, retry_after: Duration) -> bool {
    tokio::select! {
        _ = wallet_ready.notified() => false,
        _ = tokio::time::sleep(retry_after) => false,
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown requested before a wallet was configured");
            true
        }
    }
}

// ── 30s fallback re-evaluation ────────────────────────────────────────────────

const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn evaluate_all_markets(
    state: &Arc<RwLock<AppState>>,
    executor: &Arc<Executor>,
    alerter: &Arc<Alerter>,
) {
    // Reset any Cancelling states that have been stuck for longer than CANCEL_TIMEOUT.
    // This recovers from hung HTTP cancel calls so the quoter can re-evaluate the market.
    {
        let mut s = state.write().await;
        for status in s.order_status.values_mut() {
            if let OrderStatus::Cancelling { order_id, since } = status {
                if since.elapsed() > CANCEL_TIMEOUT {
                    warn!("Cancel for order {} timed out — reverting to Idle", order_id);
                    *status = OrderStatus::Idle;
                }
            }
        }
    }

    let actions: Vec<(String, QuoteAction, String, rust_decimal::Decimal)> = {
        let s = state.read().await;
        s.configs.iter().filter_map(|c| {
            let book = s.books.get(&c.token_id)?;
            let status = s.order_status.get(&c.id).cloned().unwrap_or_default();
            let action = quoter::evaluate(c, book, &status);
            Some((c.id.clone(), action, c.token_id.clone(), c.order_size))
        }).collect()
    };

    for (config_id, action, token_id, order_size) in actions {
        match action {
            QuoteAction::Hold => {}

            QuoteAction::Place { price } => {
                {
                    let mut s = state.write().await;
                    if s.heartbeat_paused { continue; }
                    let status = s.order_status.entry(config_id.clone()).or_default();
                    if *status != OrderStatus::Idle { continue; }
                    *status = OrderStatus::Placing { price };
                }
                let exec2  = Arc::clone(executor);
                let state2 = Arc::clone(state);
                let alert2 = Arc::clone(alerter);
                tokio::spawn(async move {
                    let shares = match Executor::shares_from_usd(order_size, price) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("Timer place: invalid price {}: {}", price, e);
                            let mut s = state2.write().await;
                            if matches!(s.order_status.get(&config_id), Some(OrderStatus::Placing { .. })) {
                                s.order_status.insert(config_id, OrderStatus::Idle);
                            }
                            return;
                        }
                    };
                    match exec2.place_buy_order(&token_id, price, shares).await {
                        Ok(oid) => {
                            let mut s = state2.write().await;
                            let still_placing = matches!(
                                s.order_status.get(&config_id),
                                Some(OrderStatus::Placing { price: p }) if *p == price
                            );
                            if still_placing {
                                let (label, token_label) = s.configs.iter()
                                    .find(|c| c.id == config_id)
                                    .map(|c| (c.label.clone(), c.token_label.clone()))
                                    .unwrap_or_else(|| ("unknown".to_string(), "?".to_string()));
                                s.order_status.insert(config_id.clone(), OrderStatus::Live { order_id: oid, price });
                                s.place_failures.remove(&config_id); // reset on success
                                drop(s);
                                alert2.info(format!(
                                    "Order Placed: `{}`\nBUY {} {} Shares @ {}c for {}$",
                                    label,
                                    shares,
                                    token_label,
                                    price * rust_decimal_macros::dec!(100),
                                    order_size
                                ));
                            } else {
                                drop(s);
                                warn!("Timer: state changed during placement — cancelling orphan {}", oid);
                                let _ = exec2.cancel_order(&oid).await;
                            }
                        }
                        Err(e) => {
                            let err_str = e.to_string();

                            // "Not enough balance" = collateral locked by a matching/settling order.
                            // Not a config failure — just wait for next evaluation cycle.
                            if err_str.contains("not enough balance") {
                                warn!("Timer place skipped: insufficient balance (order settling?) — will retry on next tick");
                                let mut s = state2.write().await;
                                if matches!(s.order_status.get(&config_id), Some(OrderStatus::Placing { .. })) {
                                    s.order_status.insert(config_id, OrderStatus::Idle);
                                }
                                return;
                            }

                            error!("Timer place failed: {}", e);
                            alert2.error(format!("Timer place failed: {}", e));

                            let auto_remove = {
                                let mut s = state2.write().await;
                                if !matches!(s.order_status.get(&config_id), Some(OrderStatus::Placing { .. })) {
                                    None
                                } else {
                                    s.order_status.insert(config_id.clone(), OrderStatus::Idle);
                                    let failures = s.place_failures.entry(config_id.clone()).or_insert(0);
                                    *failures += 1;
                                    if *failures < MAX_PLACE_FAILURES {
                                        None
                                    } else {
                                        let label = s.configs.iter()
                                            .find(|c| c.id == config_id)
                                            .map(|c| c.label.clone())
                                            .unwrap_or_else(|| config_id.to_string());
                                        let remaining: Vec<_> = s.configs.iter()
                                            .filter(|c| c.id != config_id)
                                            .cloned()
                                            .collect();
                                        let markets_file = s.markets_file.clone();
                                        Some((label, remaining, markets_file))
                                    }
                                }
                            };

                            if let Some((label, remaining, markets_file)) = auto_remove {
                                match save_markets(&markets_file, &remaining) {
                                    Ok(_) => {
                                        let mut s = state2.write().await;
                                        s.configs.retain(|c| c.id != config_id);
                                        s.order_status.remove(&config_id);
                                        s.place_failures.remove(&config_id);
                                        drop(s);
                                        alert2.error(format!(
                                            "Market '{}' auto-removed after {} consecutive placement failures",
                                            label, MAX_PLACE_FAILURES
                                        ));
                                    }
                                    Err(e) => {
                                        error!("Failed to save markets after auto-remove of '{}': {}", label, e);
                                        alert2.error(format!(
                                            "Market '{}' hit {} failures but save failed — will retry on next failure: {}",
                                            label, MAX_PLACE_FAILURES, e
                                        ));
                                    }
                                }
                            }
                        }
                    }
                });
            }

            QuoteAction::Cancel { order_id, .. } => {
                let exec2  = Arc::clone(executor);
                let state2 = Arc::clone(state);
                {
                    let mut s = state.write().await;
                    s.order_status.insert(config_id.clone(), OrderStatus::Cancelling {
                        order_id: order_id.clone(),
                        since: std::time::Instant::now(),
                    });
                }
                tokio::spawn(async move {
                    let confirmed = match exec2.cancel_order_verified(&order_id).await {
                        Ok(true) => true,
                        Ok(false) => {
                            warn!("Timer cancel: order {} still on CLOB after verified cancel — leaving Cancelling for timeout recovery", order_id);
                            false
                        }
                        Err(e) => {
                            error!("Timer cancel order {} failed: {}", order_id, e);
                            false // leave Cancelling; 30s timeout recovers
                        }
                    };
                    if confirmed {
                        let mut s = state2.write().await;
                        if matches!(s.order_status.get(&config_id),
                            Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == &order_id)
                        {
                            s.order_status.insert(config_id, OrderStatus::Idle);
                        }
                    }
                });
            }

            QuoteAction::Replace { order_id, new_price } => {
                {
                    let mut s = state.write().await;
                    if s.heartbeat_paused { continue; }
                    s.order_status.insert(config_id.clone(), OrderStatus::Cancelling {
                        order_id: order_id.clone(),
                        since: std::time::Instant::now(),
                    });
                }
                let exec2  = Arc::clone(executor);
                let state2 = Arc::clone(state);
                let alert2 = Arc::clone(alerter);
                tokio::spawn(async move {
                    // Ok(true)  = confirmed off CLOB — safe to place.
                    // Ok(false) = verified still active — leave Cancelling; 30s timeout recovers.
                    // Err(_)    = network failure — leave Cancelling; 30s timeout recovers.
                    match exec2.cancel_order_verified(&order_id).await {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!("Timer replace: cancel of {} verified still live — leaving Cancelling for timeout recovery", order_id);
                            return; // leave state as Cancelling; 30s timeout will reset to Idle
                        }
                        Err(e) => {
                            error!("Timer replace: cancel {} failed — aborting replace: {}", order_id, e);
                            return; // leave Cancelling; 30s timeout recovers
                        }
                    }

                    // Re-read the book for a fresh target — market may have moved during cancel.
                    let (fresh_price, fresh_size) = {
                        let s = state2.read().await;
                        match (s.books.get(&token_id), s.configs.iter().find(|c| c.id == config_id)) {
                            (Some(book), Some(cfg)) => {
                                match book.best_bid {
                                    Some(bb) => {
                                        use crate::engine::orderbook::TokenBook;
                                        use rust_decimal_macros::dec;
                                        let raw = bb - cfg.distance;
                                        if raw <= dec!(0) {
                                            drop(s);
                                            let mut s = state2.write().await;
                                            if matches!(s.order_status.get(&config_id),
                                                Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == &order_id)
                                            {
                                                s.order_status.insert(config_id, OrderStatus::Idle);
                                            }
                                            return;
                                        }
                                        (TokenBook::snap_to_tick(raw, cfg.tick_size), cfg.order_size)
                                    }
                                    None => (new_price, order_size),
                                }
                            }
                            _ => (new_price, order_size),
                        }
                    };

                    let shares = match Executor::shares_from_usd(fresh_size, fresh_price) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("Timer replace: invalid price {}: {}", fresh_price, e);
                            let mut s = state2.write().await;
                            if matches!(s.order_status.get(&config_id),
                                Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == &order_id)
                            {
                                s.order_status.insert(config_id, OrderStatus::Idle);
                            }
                            return;
                        }
                    };
                    match exec2.place_buy_order(&token_id, fresh_price, shares).await {
                        Ok(oid) => {
                            let mut s = state2.write().await;
                            let still_cancelling = matches!(
                                s.order_status.get(&config_id),
                                Some(OrderStatus::Cancelling { order_id: existing, .. }) if existing == &order_id
                            );
                            if still_cancelling {
                                let (label, token_label) = s.configs.iter()
                                    .find(|c| c.id == config_id)
                                    .map(|c| (c.label.clone(), c.token_label.clone()))
                                    .unwrap_or_else(|| ("unknown".to_string(), "?".to_string()));
                                s.order_status.insert(config_id, OrderStatus::Live { order_id: oid, price: fresh_price });
                                drop(s);
                                alert2.info(format!(
                                    "Order Replaced: `{}`\nBUY {} {} Shares @ {}c for {}$",
                                    label,
                                    shares,
                                    token_label,
                                    fresh_price * rust_decimal_macros::dec!(100),
                                    order_size
                                ));
                            } else {
                                drop(s);
                                warn!("Timer replace: state changed during cancel+place — cancelling orphan {}", oid);
                                let _ = exec2.cancel_order(&oid).await;
                            }
                        }
                        Err(e) => {
                            error!("Timer replace failed: {}", e);
                            alert2.error(format!("Timer replace failed: {}", e));
                            let mut s = state2.write().await;
                            if matches!(s.order_status.get(&config_id),
                                Some(OrderStatus::Cancelling { order_id: existing, .. }) if existing == &order_id)
                            {
                                s.order_status.insert(config_id, OrderStatus::Idle);
                            }
                        }
                    }
                });
            }

            QuoteAction::Deactivate { order_id, reason } => {
                use crate::engine::quoter::DeactivateReason;

                // Auto-pause config and save to disk for Volatility triggers.
                if matches!(reason, DeactivateReason::Volatility) {
                    let (markets_file, label) = {
                        let mut s = state.write().await;
                        let label = s.configs.iter()
                            .find(|c| c.id == config_id)
                            .map(|c| c.label.clone())
                            .unwrap_or_else(|| config_id.clone());
                        if let Some(cfg) = s.configs.iter_mut().find(|c| c.id == config_id) {
                            cfg.paused = true;
                        }
                        (s.markets_file.clone(), label)
                    };
                    let configs = state.read().await.configs.clone();
                    if let Err(e) = save_markets(&markets_file, &configs) {
                        error!("Failed to save markets after volatility pause of '{}': {}", label, e);
                    }
                    alerter.warn(format!(
                        "Market Auto-Paused: `{}`\nVolatility threshold exceeded — use /resume-market to re-enable",
                        label
                    ));
                } else {
                    let label = state.read().await.configs.iter()
                        .find(|c| c.id == config_id)
                        .map(|c| c.label.clone())
                        .unwrap_or_else(|| config_id.clone());
                    let (title, msg) = match reason {
                        DeactivateReason::Paused     => ("Market Paused", "Order cancelled — use /resume-market to re-enable"),
                        DeactivateReason::Expired    => ("Market Expired", "Config expired and has been deactivated"),
                        DeactivateReason::Volatility => unreachable!(),
                    };
                    alerter.info(format!("{}: `{}`\n{}", title, label, msg));
                }

                match order_id {
                    Some(oid) => {
                        {
                            let mut s = state.write().await;
                            s.order_status.insert(config_id.clone(), OrderStatus::Cancelling {
                                order_id: oid.clone(),
                                since: std::time::Instant::now(),
                            });
                        }
                        let exec2  = Arc::clone(executor);
                        let state2 = Arc::clone(state);
                        tokio::spawn(async move {
                            let _ = exec2.cancel_order_verified(&oid).await;
                            let mut s = state2.write().await;
                            if matches!(s.order_status.get(&config_id),
                                Some(OrderStatus::Cancelling { order_id: existing, .. }) if existing == &oid)
                            {
                                s.order_status.insert(config_id, OrderStatus::Idle);
                            }
                        });
                    }
                    None => {
                        // No real order exists yet (Idle or Placing).
                        // Reset to Idle — if Placing, the in-flight task will cancel the orphan.
                        let mut s = state.write().await;
                        s.order_status.insert(config_id, OrderStatus::Idle);
                    }
                }
            }
        }
    }
}
