//! Application orchestration: boots the engine tasks + the web dashboard,
//! then waits for shutdown and cancels open orders.

use crate::config::Config;
use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::engine::quoter::QuoteAction;
use crate::engine::ws_manager::{AppState, MAX_PLACE_FAILURES};
use crate::engine::{heartbeat, quoter, ws_manager};
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

/// Keep secrets out of crash dumps: the process holds the decrypted wallet key.
/// No core files, and (on Linux) not dumpable, which also stops other processes
/// of the same user from attaching with ptrace or reading `/proc/<pid>/mem`.
#[cfg(unix)]
fn harden_process() {
    let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain syscalls with valid arguments; no memory is shared or retained.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
        #[cfg(target_os = "linux")]
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
}

#[cfg(not(unix))]
fn harden_process() {}

pub async fn run() -> Result<()> {
    harden_process();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("polyfarmer=info")),
        )
        .init();

    info!("================================");
    info!("   Polyfarmer starting");
    info!("================================");

    let config = Arc::new(Config::from_env()?);

    // ── Credential store ──────────────────────────────────────────────────────
    let store = Arc::new(crate::creds::CredentialStore::open(config.data_dir.clone())?);
    if let Some(code) = store.setup_code() {
        // Shown here (and saved as <data>/setup.code) because the first-run
        // page requires it: whoever can read this log owns the install.
        info!("┌──────────────────────────────────────────────────────────────");
        info!("│ First run — create your admin password:");
        info!("│   setup code:  {code}");
        info!("│   open:        {}", setup_link(&config.dashboard_bind, &code));
        info!("└──────────────────────────────────────────────────────────────");
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

    // Live alert fan-out: engine publishes here, the Activity feed's SSE
    // endpoint subscribes. Buffered so a briefly-slow client doesn't block.
    let (alert_tx, _) = tokio::sync::broadcast::channel::<crate::types::Alert>(256);
    let alerter = Arc::new(Alerter::new(&config.alerts_file, alert_tx.clone()));

    // Lets the dashboard poke the quote loop to re-evaluate immediately (e.g.
    // right after a resume) instead of waiting for the next tick.
    let quote_nudge = Arc::new(tokio::sync::Notify::new());

    // ── Spawn: web dashboard (reachable even before the wallet is configured) ──
    // Returns a handle the boot task awaits, so configuring a wallet in the
    // dashboard starts the engine immediately — no restart on first run.
    let (wallet_ready, engine_handle) = {
        let bind = config.dashboard_bind.clone();
        match tokio::net::TcpListener::bind(&bind).await {
            Ok(listener) => {
                info!("Dashboard on http://{}", bind);
                for w in crate::config::exposure_warnings(&bind, config.secure_cookies) {
                    warn!("{w}");
                }
                let web_state = crate::web::WebState::with_config(
                    Arc::clone(&store),
                    Arc::clone(&state),
                    config.polygon_rpc_url.clone(),
                    config.reward_history_file.clone(),
                    alert_tx.clone(),
                    config.alerts_file.clone(),
                    Arc::clone(&quote_nudge),
                )
                .with_secure_cookies(config.secure_cookies);
                let ready = Arc::clone(&web_state.wallet_ready);
                let handle = web_state.engine_handle.clone();
                // Show the configured wallet's public address to read-only
                // dashboard lookups (positions / fills) from the start.
                if let Ok(Some(w)) = store.load_wallet() {
                    web_state.set_wallet_address(Some(w.proxy_wallet));
                }
                crate::web::events::spawn_state_watcher(web_state.clone());
                crate::web::prewarm_browse();
                let svc = crate::web::router(web_state).into_make_service_with_connect_info::<std::net::SocketAddr>();
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
                if wait_or_exit(&wallet_ready, WALLET_POLL).await {
                    return Ok(());
                }
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
                if wait_or_exit(&wallet_ready, START_RETRY).await {
                    return Ok(());
                }
                continue;
            }
        };

        set_phase(&state, EnginePhase::Starting).await;

        let executor = match Executor::new(creds.expose_key(), proxy_wallet, config.polygon_rpc_url.as_deref()).await {
            Ok(e) => Arc::new(e),
            Err(e) => {
                // `{:#}` prints the whole cause chain (DNS, connection, TLS...); the SDK's top-level text
                // alone is just "error sending request".
                let chain = format!("{e:#}");
                error!("Could not authenticate with Polymarket: {chain}");
                if !failure_alerted {
                    let hint = start_failure_hint(&chain);
                    // The terminal is where a newcomer is looking; the dashboard's Activity feed gets it too.
                    warn!("The engine couldn't start. {hint} It keeps retrying.");
                    alerter.error(format!("Wallet saved, but the engine couldn't start. {hint} It keeps retrying."));
                    failure_alerted = true;
                }
                set_phase(&state, EnginePhase::Error).await;
                if wait_or_exit(&wallet_ready, START_RETRY).await {
                    return Ok(());
                }
                continue;
            }
        };

        // ── Startup safety: cancel any open orders on tracked markets ─────────
        let startup_tokens: Vec<String> = {
            let s = state.read().await;
            s.configs.iter().map(|c| c.token_id.clone()).collect::<std::collections::HashSet<_>>().into_iter().collect()
        };
        if let Err(e) = executor.cancel_orders_for_tokens(&startup_tokens).await {
            error!("Startup cancel failed — refusing to trade with potentially open orders: {}", e);
            if !failure_alerted {
                alerter.error(
                    "Engine start aborted: could not clear existing orders. It will keep retrying — check logs.",
                );
                failure_alerted = true;
            }
            set_phase(&state, EnginePhase::Error).await;
            if wait_or_exit(&wallet_ready, START_RETRY).await {
                return Ok(());
            }
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
    ws_manager::spawn(Arc::clone(&state), Arc::clone(&executor), Arc::clone(&alerter), ws_cmd_rx, stop_rx.clone());

    // ── Spawn: Heartbeat ──────────────────────────────────────────────────────
    heartbeat::spawn(Arc::clone(&executor), Arc::clone(&alerter), Arc::clone(&state), stop_rx.clone());

    // ── Spawn: daily reward-history snapshot ──────────────────────────────────
    // markets.json needs no poll loop — the web handlers are the only write path
    // and mutate `AppState` directly; ws_manager's
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
        let state2 = Arc::clone(&state);
        let executor2 = Arc::clone(&executor);
        let alerter2 = Arc::clone(&alerter);
        let nudge = Arc::clone(&quote_nudge);
        let mut stop = stop_rx.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(QUOTE_TIMER_INTERVAL);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        evaluate_all_markets(&state2, &executor2, &alerter2).await;
                    }
                    // Dashboard poke (e.g. resume): re-evaluate now, don't wait.
                    _ = nudge.notified() => {
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
        let state2 = Arc::clone(&state);
        let alerter2 = Arc::clone(&alerter);
        let mut stop = stop_rx.clone();

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

    // ── Graceful shutdown on SIGINT (Ctrl+C) / SIGTERM (systemd, `kill`, docker) ──
    shutdown_signal().await;
    info!("Shutdown signal received — cancelling bot orders...");
    alerter.warn("Bot shutting down — cancelling open orders");

    let _ = stop_tx.send(true);

    // Include Cancelling — the in-flight cancel may not have completed before shutdown.
    let live_order_ids: Vec<String> = {
        let s = state.read().await;
        s.order_status
            .values()
            .filter_map(|status| match status {
                OrderStatus::Live { order_id, .. } | OrderStatus::Cancelling { order_id, .. } => Some(order_id.clone()),
                _ => None,
            })
            .collect()
    };
    if let Err(e) = executor.cancel_orders(&live_order_ids).await {
        error!("Shutdown cancel failed: {}", e);
    }

    info!("Shutdown complete");
    Ok(())
}

// ── Boot helpers ────────────────────────────────────────────────────────────

/// Set the engine phase on shared state (brief write lock, released at once).
/// What to tell the user when signing in to Polymarket fails. `chain` is the full error text.
fn start_failure_hint(chain: &str) -> &'static str {
    if chain.to_ascii_lowercase().contains("certificate") {
        "This is what it usually looks like when your country or ISP blocks Polymarket: the block answers with a \
         certificate that isn't Polymarket's. A VPN or proxy, a company firewall or antivirus software that scans \
         HTTPS can cause the same error. Polyfarmer can't work around a block."
    } else {
        "Usually this is a network problem: check your internet connection and any VPN, proxy or firewall. \
         Polymarket also restricts some regions and may block VPNs."
    }
}

async fn set_phase(state: &Arc<RwLock<AppState>>, phase: EnginePhase) {
    state.write().await.engine_phase = phase;
}

/// `http://host:port/welcome?code=…` for the startup log. A wildcard bind
/// (`0.0.0.0`) has no usable host, so show `localhost`; on a remote server the
/// owner substitutes the address they reach it by.
fn setup_link(bind: &str, code: &str) -> String {
    let host = bind
        .strip_prefix("0.0.0.0")
        .or_else(|| bind.strip_prefix("[::]"))
        .map(|rest| format!("localhost{rest}"))
        .unwrap_or_else(|| bind.to_string());
    format!("http://{host}/welcome?code={code}")
}

/// Resolves on SIGINT (Ctrl+C) or, on Unix, SIGTERM — what `systemctl stop`,
/// `kill` and `docker stop` send. Handling only Ctrl+C meant a service stop
/// killed the process without cancelling its resting orders on Polymarket.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            // Couldn't register SIGTERM — still honour Ctrl+C rather than never exiting.
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
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
        _ = shutdown_signal() => {
            info!("Shutdown requested before a wallet was configured");
            true
        }
    }
}

// ── 30s fallback re-evaluation ────────────────────────────────────────────────

const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn evaluate_all_markets(state: &Arc<RwLock<AppState>>, executor: &Arc<Executor>, alerter: &Arc<Alerter>) {
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
        s.configs
            .iter()
            .filter_map(|c| {
                let book = s.books.get(&c.token_id)?;
                let status = s.order_status.get(&c.id).cloned().unwrap_or_default();
                let action = quoter::evaluate(c, book, &status);
                Some((c.id.clone(), action, c.token_id.clone(), c.order_size))
            })
            .collect()
    };

    for (config_id, action, token_id, order_size) in actions {
        match action {
            QuoteAction::Hold => {}

            QuoteAction::Place { price } => {
                {
                    let mut s = state.write().await;
                    if s.heartbeat_paused {
                        continue;
                    }
                    let status = s.order_status.entry(config_id.clone()).or_default();
                    if *status != OrderStatus::Idle {
                        continue;
                    }
                    *status = OrderStatus::Placing { price };
                }
                let exec2 = Arc::clone(executor);
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
                                let (label, token_label) = s
                                    .configs
                                    .iter()
                                    .find(|c| c.id == config_id)
                                    .map(|c| (c.label.clone(), c.token_label.clone()))
                                    .unwrap_or_else(|| ("unknown".to_string(), "?".to_string()));
                                s.order_status.insert(config_id.clone(), OrderStatus::Live { order_id: oid, price });
                                s.place_failures.remove(&config_id); // reset on success
                                drop(s);
                                alert2.info(format!(
                                    "Order placed · {}\nBUY {} {} shares @ {}¢ (${})",
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
                            alert2.error(format!("Order placement failed: {}", e));

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
                                        let label = s
                                            .configs
                                            .iter()
                                            .find(|c| c.id == config_id)
                                            .map(|c| c.label.clone())
                                            .unwrap_or_else(|| config_id.to_string());
                                        let remaining: Vec<_> =
                                            s.configs.iter().filter(|c| c.id != config_id).cloned().collect();
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
                let exec2 = Arc::clone(executor);
                let state2 = Arc::clone(state);
                {
                    let mut s = state.write().await;
                    s.order_status.insert(
                        config_id.clone(),
                        OrderStatus::Cancelling { order_id: order_id.clone(), since: std::time::Instant::now() },
                    );
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
                    if s.heartbeat_paused {
                        continue;
                    }
                    s.order_status.insert(
                        config_id.clone(),
                        OrderStatus::Cancelling { order_id: order_id.clone(), since: std::time::Instant::now() },
                    );
                }
                let exec2 = Arc::clone(executor);
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
                            (Some(book), Some(cfg)) => match book.best_bid {
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
                            },
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
                                let (label, token_label) = s
                                    .configs
                                    .iter()
                                    .find(|c| c.id == config_id)
                                    .map(|c| (c.label.clone(), c.token_label.clone()))
                                    .unwrap_or_else(|| ("unknown".to_string(), "?".to_string()));
                                s.order_status
                                    .insert(config_id, OrderStatus::Live { order_id: oid, price: fresh_price });
                                drop(s);
                                alert2.info(format!(
                                    "Order replaced · {}\nBUY {} {} shares @ {}¢ (${})",
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
                            alert2.error(format!("Replace order failed: {}", e));
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
                        let label = s
                            .configs
                            .iter()
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
                        "Auto-paused · {}\nBest bid moved past your volatility limit — resume from Markets when stable",
                        label
                    ));
                } else {
                    let label = state
                        .read()
                        .await
                        .configs
                        .iter()
                        .find(|c| c.id == config_id)
                        .map(|c| c.label.clone())
                        .unwrap_or_else(|| config_id.clone());
                    let (title, msg) = match reason {
                        DeactivateReason::Paused => ("Paused", "Order cancelled — resume from Markets"),
                        DeactivateReason::Expired => ("Expired", "Expiry reached — stopped quoting"),
                        DeactivateReason::Volatility => unreachable!(),
                    };
                    alerter.info(format!("{} · {}\n{}", title, label, msg));
                }

                match order_id {
                    Some(oid) => {
                        {
                            let mut s = state.write().await;
                            s.order_status.insert(
                                config_id.clone(),
                                OrderStatus::Cancelling { order_id: oid.clone(), since: std::time::Instant::now() },
                            );
                        }
                        let exec2 = Arc::clone(executor);
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

#[cfg(test)]
mod link_tests {
    use super::setup_link;

    #[test]
    fn setup_link_uses_a_reachable_host() {
        assert_eq!(setup_link("127.0.0.1:8080", "abcd-efgh"), "http://127.0.0.1:8080/welcome?code=abcd-efgh");
        assert_eq!(setup_link("0.0.0.0:8080", "abcd-efgh"), "http://localhost:8080/welcome?code=abcd-efgh");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::shutdown_signal;
    use std::time::Duration;

    #[test]
    fn start_failure_hints_distinguish_certificate_errors_from_other_network_problems() {
        let cert = "Internal: error sending request: client error (Connect): invalid peer certificate: NotValidForName";
        let hint = super::start_failure_hint(cert);
        assert!(
            hint.contains("country or ISP blocks Polymarket") && hint.contains("certificate that isn't Polymarket's")
        );
        let other = "Internal: error sending request for url (https://clob.polymarket.com/auth/api-key)";
        let hint = super::start_failure_hint(other);
        assert!(hint.contains("network problem") && hint.contains("VPN") && hint.contains("restricts some regions"));
    }

    /// A SIGTERM (what a service stop sends) must wake `shutdown_signal`, so the
    /// existing cancel-open-orders shutdown path runs instead of the process
    /// dying with orders still resting.
    #[tokio::test]
    async fn sigterm_triggers_graceful_shutdown() {
        let waiter = tokio::spawn(shutdown_signal());
        // Let the task register its handler before the signal arrives.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // SAFETY: raising a signal in our own process; tokio's handler (installed
        // above and kept for the process lifetime) consumes it.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        tokio::time::timeout(Duration::from_secs(3), waiter)
            .await
            .expect("shutdown_signal should resolve after SIGTERM")
            .expect("task should not panic");
    }

    /// Core dumps (which would contain the decrypted wallet key) are disabled and,
    /// on Linux, the process is marked non-dumpable.
    #[cfg(target_os = "linux")]
    #[test]
    fn harden_process_disables_core_dumps_and_ptrace() {
        super::harden_process();
        // SAFETY: read-only queries about our own process.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0), 0);
            let mut lim = libc::rlimit { rlim_cur: 1, rlim_max: 1 };
            assert_eq!(libc::getrlimit(libc::RLIMIT_CORE, &mut lim), 0);
            assert_eq!((lim.rlim_cur, lim.rlim_max), (0, 0));
        }
    }
}
