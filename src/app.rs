//! Application orchestration: boots the engine tasks + the web dashboard,
//! then waits for shutdown and cancels open orders.

use crate::config::Config;
use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::engine::quoter::QuoteAction;
use crate::engine::{heartbeat, quoter, ws_manager};
use crate::engine::ws_manager::{AppState, MAX_PLACE_FAILURES};
use crate::storage::{load_markets, save_markets};
use crate::types::{MarketConfig, OrderStatus, WsCommand};

use eyre::Result;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, RwLock};
use tracing::{error, info, warn};

const MARKETS_POLL_INTERVAL: Duration = Duration::from_secs(3);
const QUOTE_TIMER_INTERVAL: Duration = Duration::from_secs(30);
const HOURLY_SUMMARY_INTERVAL: Duration = Duration::from_secs(3600);

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

    // ── Credential store + first-run admin password ───────────────────────────
    let store = Arc::new(crate::creds::CredentialStore::open(config.data_dir.clone())?);
    if !store.is_initialized() {
        let pw = store.init_admin()?;
        info!("══════════════════════════════════════════════════════");
        info!("  First-run admin password:  {}", pw);
        info!("  Open the dashboard and change it on first login.");
        info!("══════════════════════════════════════════════════════");
    }

    // ── Spawn: web dashboard (reachable even before the wallet is configured) ──
    {
        let bind = config.dashboard_bind.clone();
        match tokio::net::TcpListener::bind(&bind).await {
            Ok(listener) => {
                info!("Dashboard on http://{}", bind);
                let web_state = crate::web::WebState::new(Arc::clone(&store));
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, crate::web::router(web_state)).await {
                        error!("Web server error: {}", e);
                    }
                });
            }
            Err(e) => error!("Failed to bind dashboard on {}: {}", bind, e),
        }
    }

    // ── Wallet credentials (entered via dashboard, encrypted at rest) ──────────
    let creds = match store.load_wallet()? {
        Some(c) => c,
        None => {
            warn!(
                "No wallet configured — open the dashboard at http://{} to set it up, \
                 then restart to begin trading.",
                config.dashboard_bind
            );
            tokio::signal::ctrl_c().await?;
            info!("Shutdown complete");
            return Ok(());
        }
    };
    let proxy_wallet: alloy::primitives::Address = creds
        .proxy_wallet
        .parse()
        .map_err(|_| eyre::eyre!("stored proxy wallet is not a valid address"))?;

    let alerter = Arc::new(Alerter::new(&config.alerts_file));
    let executor = Arc::new(Executor::new(creds.expose_key(), proxy_wallet).await?);

    // ── Load initial markets ──────────────────────────────────────────────────
    let initial_configs = load_markets(&config.markets_file)?;
    info!("Loaded {} market configs", initial_configs.len());

    // ── Startup: cancel any open orders on tracked markets ────────────────────
    {
        let startup_tokens: Vec<String> = initial_configs.iter()
            .map(|c| c.token_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        executor.cancel_orders_for_tokens(&startup_tokens).await.map_err(|e| {
            eyre::eyre!("Startup cancel failed — refusing to start with potentially open orders: {}", e)
        })?;
    }

    let state = Arc::new(RwLock::new({
        let mut s = AppState::new(config.markets_file.clone());
        for cfg in &initial_configs {
            s.order_status.insert(cfg.id.clone(), OrderStatus::Idle);
        }
        s.configs = initial_configs;
        s
    }));

    // ── Channels ──────────────────────────────────────────────────────────────
    let (ws_cmd_tx, ws_cmd_rx) = mpsc::channel::<WsCommand>(20);
    let (stop_tx, stop_rx) = watch::channel(false);

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

    // ── Spawn: markets.json watcher ───────────────────────────────────────────
    {
        let state2    = Arc::clone(&state);
        let alerter2  = Arc::clone(&alerter);
        let executor2 = Arc::clone(&executor);
        let config2   = Arc::clone(&config);
        let cmd_tx    = ws_cmd_tx.clone();
        let mut stop  = stop_rx.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(MARKETS_POLL_INTERVAL);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        poll_markets(&config2, &state2, &executor2, &alerter2, &cmd_tx).await;
                    }
                    _ = stop.changed() => {
                        if *stop.borrow() { break; }
                    }
                }
            }
        });
    }

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

// ── Markets watcher ───────────────────────────────────────────────────────────

async fn poll_markets(
    config: &Config,
    state: &Arc<RwLock<AppState>>,
    executor: &Arc<Executor>,
    alerter: &Arc<Alerter>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) {
    let new_configs = match load_markets(&config.markets_file) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to load markets.json: {}", e);
            alerter.warn(format!("Failed to load markets.json — bot is not re-evaluating markets: {}", e));
            return;
        }
    };

    // Compute diffs and mutate state under the write lock, then release before
    // sending channel commands (avoids holding the lock during an async send).
    let (new_tokens, tokens_to_unsub, orders_to_cancel) = {
        let mut s = state.write().await;

        let old_ids: HashSet<String> = s.configs.iter().map(|c| c.id.clone()).collect();
        let new_ids: HashSet<String> = new_configs.iter().map(|c| c.id.clone()).collect();

        // Added configs
        let added: Vec<&MarketConfig> = new_configs.iter()
            .filter(|c| !old_ids.contains(&c.id))
            .collect();

        for cfg in &added {
            s.order_status.insert(cfg.id.clone(), OrderStatus::Idle);
            alerter.info(format!("Market added: {}", cfg.label));
        }

        // New token_ids to subscribe (deduplicated against already-subscribed)
        let existing_tokens: HashSet<String> = s.configs.iter()
            .map(|c| c.token_id.clone()).collect();
        let new_tokens: Vec<String> = added.iter()
            .filter(|c| !existing_tokens.contains(&c.token_id))
            .map(|c| c.token_id.clone())
            .collect();

        // Removed configs
        let removed: Vec<MarketConfig> = s.configs.iter()
            .filter(|c| !new_ids.contains(&c.id))
            .cloned()
            .collect();

        // Collect order IDs that need cancellation from removed markets.
        // Include both Live and Cancelling — Cancelling may not have completed.
        let mut orders_to_cancel: Vec<String> = Vec::new();
        for cfg in &removed {
            let order_id = match s.order_status.get(&cfg.id) {
                Some(OrderStatus::Live { order_id, .. }) => {
                    warn!("Market removed with live order — cancelling: {}", cfg.label);
                    alerter.warn(format!("Market removed: {} (cancelling live order)", cfg.label));
                    Some(order_id.clone())
                }
                Some(OrderStatus::Cancelling { order_id, .. }) => {
                    warn!("Market removed while order cancel in-flight — re-cancelling: {}", cfg.label);
                    alerter.warn(format!("Market removed: {} (re-cancelling in-flight order)", cfg.label));
                    Some(order_id.clone())
                }
                _ => {
                    alerter.info(format!("Market removed: {}", cfg.label));
                    None
                }
            };
            if let Some(oid) = order_id {
                orders_to_cancel.push(oid);
            }
            s.order_status.remove(&cfg.id);
            s.place_failures.remove(&cfg.id);
        }

        // Token_ids no longer referenced by any config
        let remaining_tokens: HashSet<String> = new_configs.iter()
            .map(|c| c.token_id.clone()).collect();
        let tokens_to_unsub: Vec<String> = s.configs.iter()
            .filter(|c| !remaining_tokens.contains(&c.token_id))
            .map(|c| c.token_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        s.configs = new_configs;
        (new_tokens, tokens_to_unsub, orders_to_cancel)
    }; // write lock released here

    // Send channel commands outside the lock — send().await blocks if the channel
    // is full, so we must not hold the state lock while waiting.
    if !new_tokens.is_empty() {
        if cmd_tx.send(WsCommand::Subscribe(new_tokens)).await.is_err() {
            warn!("WS command channel closed — subscribe dropped");
        }
    }
    if !tokens_to_unsub.is_empty() {
        if cmd_tx.send(WsCommand::Unsubscribe(tokens_to_unsub)).await.is_err() {
            warn!("WS command channel closed — unsubscribe dropped");
        }
    }

    // Cancel orders from removed markets outside the lock (network I/O).
    if !orders_to_cancel.is_empty() {
        if let Err(e) = executor.cancel_orders(&orders_to_cancel).await {
            error!("Failed to cancel orders for removed markets: {}", e);
            alerter.error(format!("Failed to cancel orders after market removal — manual check required: {}", e));
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
