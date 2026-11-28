//! Application orchestration: boots the engine tasks + the web dashboard,
//! then waits for shutdown and cancels open orders.

use crate::config::Config;
use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::engine::placer::OrderApi;
use crate::engine::ws_manager::AppState;
use crate::engine::{heartbeat, pairing, placer, runner, ws_manager};
use crate::storage::load_markets;
use crate::types::{EnginePhase, OrderStatus, WsCommand};

use eyre::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Notify, RwLock};
use tracing::{error, info, warn};

/// Shutdown: how long placements already in flight may take to land.
const SHUTDOWN_SETTLE: Duration = Duration::from_secs(5);
/// Shutdown: the limit for each of the two cancel steps. 5 + 20 + 20 stays
/// inside the service's 60 s stop timeout (deploy/polyfarmer.service).
const SHUTDOWN_CANCEL_LIMIT: Duration = Duration::from_secs(20);
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
// The one place product code needs `unsafe`: these settings exist only as raw libc calls.
#[allow(unsafe_code)]
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
    // Listen for SIGTERM / Ctrl+C from the very start, once, for the whole run:
    // a signal that arrives while the bot is busy (signing in, the startup
    // check, spawning tasks) is latched instead of lost.
    let mut shutdown = Shutdown::listen();
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
    let load_alerts = paused_on_load(&initial_configs);
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
    for message in load_alerts {
        alerter.warn(message);
    }

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
                if wait_or_exit(&wallet_ready, WALLET_POLL, &mut shutdown).await {
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
                if wait_or_exit(&wallet_ready, START_RETRY, &mut shutdown).await {
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
                if wait_or_exit(&wallet_ready, START_RETRY, &mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
        };

        // ── Startup safety: cancel any open orders on tracked markets ─────────
        let startup_tokens: Vec<String> = state.read().await.farmed_tokens();
        if let Err(e) = executor.cancel_orders_for_tokens(&startup_tokens).await {
            error!("Startup cancel failed — refusing to trade with potentially open orders: {}", e);
            if !failure_alerted {
                alerter.error(
                    "Engine start aborted: could not clear existing orders. It will keep retrying — check logs.",
                );
                failure_alerted = true;
            }
            set_phase(&state, EnginePhase::Error).await;
            if wait_or_exit(&wallet_ready, START_RETRY, &mut shutdown).await {
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
    ws_manager::spawn(
        Arc::clone(&state),
        Arc::clone(&executor),
        Arc::clone(&alerter),
        Arc::clone(&quote_nudge),
        ws_cmd_rx,
        stop_rx.clone(),
    );

    // ── Spawn: Heartbeat ──────────────────────────────────────────────────────
    heartbeat::spawn(Arc::clone(&executor), Arc::clone(&alerter), Arc::clone(&state), stop_rx.clone());

    // markets.json needs no poll loop — the web handlers are the only write path
    // and mutate `AppState` directly; ws_manager's
    // connect loop always re-derives its subscription set fresh from
    // `AppState.configs` on every (re)connect, so there's nothing left to
    // reconcile from a periodic file diff.

    // ── Spawn: 30s quote timer (fallback re-evaluation) ───────────────────────
    tokio::spawn(quote_loop(
        Arc::clone(&state),
        Arc::clone(&executor),
        Arc::clone(&alerter),
        Arc::clone(&quote_nudge),
        stop_rx.clone(),
    ));

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

    // ── Graceful shutdown: Ctrl+C, SIGTERM, or closing the window on Windows ──
    shutdown.wait().await;
    info!("Shutdown signal received — cancelling bot orders...");
    alerter.warn("Bot shutting down — cancelling open orders");

    // 1. Refuse new placements, then stop the engine's tasks.
    state.write().await.stopping = true;
    let _ = stop_tx.send(true);
    // 2. Let placements already in flight land, so they're cancelled below.
    let still_placing = settle_placements(&state, SHUTDOWN_SETTLE).await;
    if still_placing > 0 {
        warn!("Shutdown: {still_placing} placement(s) still in flight — the final sweep catches them if they land");
    }

    // 3. Cancel everything tracked. Include Cancelling — the in-flight cancel
    //    may not have completed before shutdown.
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
    match tokio::time::timeout(SHUTDOWN_CANCEL_LIMIT, executor.cancel_orders(&live_order_ids)).await {
        Ok(Ok(open)) if open.is_empty() => {}
        Ok(Ok(open)) => {
            error!("Shutdown: {} order(s) could not be cancelled: {:?}", open.len(), open);
            alerter.error(format!(
                "Shutting down with {} order(s) not confirmed cancelled — check your open orders on Polymarket.",
                open.len()
            ));
        }
        Ok(Err(e)) => error!("Shutdown cancel failed: {}", e),
        Err(_) => error!("Shutdown cancel got no answer within {}s", SHUTDOWN_CANCEL_LIMIT.as_secs()),
    }

    // 4. Sweep every farmed market for any open order the list above missed:
    //    one that landed after it, or an orphan. Same check as at startup.
    let tokens: Vec<String> = state.read().await.farmed_tokens();
    match tokio::time::timeout(SHUTDOWN_CANCEL_LIMIT, executor.cancel_orders_for_tokens(&tokens)).await {
        Ok(Ok(())) => info!("Shutdown sweep: no open orders left on farmed markets"),
        Ok(Err(e)) => {
            error!("Shutdown sweep failed: {e}");
            alerter.error(format!("Shutdown couldn't confirm all orders are gone ({e}) — check Polymarket."));
        }
        Err(_) => {
            error!("Shutdown sweep got no answer within {}s", SHUTDOWN_CANCEL_LIMIT.as_secs());
            alerter.error("Shutdown couldn't confirm all orders are gone (no answer) — check Polymarket.");
        }
    }

    info!("Shutdown complete");
    Ok(())
}

// ── Boot helpers ────────────────────────────────────────────────────────────

/// One Activity alert per leg that `load_markets` paused for an invalid value.
fn paused_on_load(configs: &[crate::types::MarketConfig]) -> Vec<String> {
    configs.iter().filter_map(|c| c.describe_problem()).map(|d| format!("Paused on load: {d}")).collect()
}

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

/// Set the engine phase on shared state (brief write lock, released at once).
async fn set_phase(state: &Arc<RwLock<AppState>>, phase: EnginePhase) {
    state.write().await.engine_phase = phase;
}

/// `http://host:port/welcome?code=…` for the startup log. A wildcard bind
/// (`0.0.0.0`) has no usable host, so show `localhost`; on a remote server the
/// owner substitutes the address they reach it by.
pub fn setup_link(bind: &str, code: &str) -> String {
    let host = bind
        .strip_prefix("0.0.0.0")
        .or_else(|| bind.strip_prefix("[::]"))
        .map(|rest| format!("localhost{rest}"))
        .unwrap_or_else(|| bind.to_string());
    format!("http://{host}/welcome?code={code}")
}

/// Resolves on Ctrl+C or whatever else asks the process to stop, so the
/// shutdown path can cancel resting orders first:
/// - Unix: SIGTERM, which `systemctl stop`, `kill` and `docker stop` send.
/// - Windows: Ctrl+Break, closing the console window, logging off and shutting
///   down. tokio holds those events open while we clean up; Windows allows a
///   closing console about 5 seconds, enough to cancel.
///
/// Handling only Ctrl+C meant a service stop, or closing the window on
/// Windows, killed the process with its orders still on Polymarket.
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
    #[cfg(windows)]
    {
        use std::future::pending;
        use tokio::signal::windows;
        // An event that can't be registered just never fires; Ctrl+C still works.
        let ctrl_break = async {
            match windows::ctrl_break() {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => pending::<()>().await,
            }
        };
        let close = async {
            match windows::ctrl_close() {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => pending::<()>().await,
            }
        };
        let logoff = async {
            match windows::ctrl_logoff() {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => pending::<()>().await,
            }
        };
        let os_shutdown = async {
            match windows::ctrl_shutdown() {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = ctrl_break => {}
            _ = close => {}
            _ = logoff => {}
            _ = os_shutdown => {}
        }
    }
    #[cfg(not(any(unix, windows)))]
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
async fn wait_or_exit(wallet_ready: &Notify, retry_after: Duration, shutdown: &mut Shutdown) -> bool {
    if shutdown.requested() {
        return true;
    }
    tokio::select! {
        _ = wallet_ready.notified() => false,
        _ = tokio::time::sleep(retry_after) => false,
        _ = shutdown.wait() => {
            info!("Shutdown requested before the engine started");
            true
        }
    }
}

/// A shutdown latch. [`Shutdown::listen`] installs the SIGTERM handler right
/// away and keeps one task waiting for SIGTERM or Ctrl+C for the whole run, so
/// a signal is remembered even when nothing is awaiting it at that moment.
/// (Creating a new handler at each wait could miss one that arrived between
/// waits, and the process would keep trading until it was killed.)
#[derive(Clone)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    pub fn listen() -> Self {
        let (tx, rx) = watch::channel(false);
        #[cfg(unix)]
        let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                match term {
                    Some(mut term) => {
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {}
                            _ = term.recv() => {}
                        }
                    }
                    None => {
                        let _ = tokio::signal::ctrl_c().await;
                    }
                }
            }
            #[cfg(not(unix))]
            shutdown_signal().await;
            let _ = tx.send(true);
        });
        Shutdown(rx)
    }

    /// A shutdown was requested (it stays requested).
    pub fn requested(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolve once a shutdown has been requested (at once if it already was).
    pub async fn wait(&mut self) {
        while !*self.0.borrow_and_update() {
            if self.0.changed().await.is_err() {
                // The listener ended without a signal: nothing more will come.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Give placements already in flight up to `limit` to land, so their orders
/// are cancelled with the rest instead of left resting after exit. That is
/// every placement call still out, including one whose leg was released
/// meanwhile (paused, expired, removed): it cancels the order it gets back.
/// New ones are refused once `AppState.stopping` is set.
async fn settle_placements(state: &RwLock<AppState>, limit: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let in_flight = {
            let s = state.read().await;
            // The runner counts every call it starts; a leg marked Placing still
            // counts too, should one have been claimed without the runner.
            let placing = s.order_status.values().filter(|st| matches!(st, OrderStatus::Placing { .. })).count();
            placing.max(s.placements_in_flight.load(std::sync::atomic::Ordering::SeqCst))
        };
        if in_flight == 0 || tokio::time::Instant::now() >= deadline {
            return in_flight;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ── 30s fallback re-evaluation ────────────────────────────────────────────────

/// The quote task: re-evaluates every market on the 30 s timer and whenever
/// something nudges it, until `stop` turns true.
async fn quote_loop<A: OrderApi>(
    state: Arc<RwLock<AppState>>,
    api: Arc<A>,
    alerter: Arc<Alerter>,
    nudge: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(QUOTE_TIMER_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                evaluate_all_markets(&state, &api, &alerter, &nudge).await;
            }
            // Dashboard poke (e.g. resume): re-evaluate now, don't wait.
            _ = nudge.notified() => {
                evaluate_all_markets(&state, &api, &alerter, &nudge).await;
            }
            _ = stop.changed() => {
                if *stop.borrow() { break; }
            }
        }
    }
}

async fn evaluate_all_markets<A: OrderApi>(
    state: &Arc<RwLock<AppState>>,
    executor: &Arc<A>,
    alerter: &Arc<Alerter>,
    nudge: &Arc<tokio::sync::Notify>,
) {
    // Retry cancels that have stayed unconfirmed past placer::CANCEL_RETRY_AFTER
    // (a hung or failed HTTP call). The leg is freed only once Polymarket confirms
    // the old order is gone, never on a timer alone.
    let stuck = placer::claim_stuck_cancels(&mut *state.write().await);
    for (config_id, order_id) in stuck {
        warn!("Cancel of order {} unconfirmed for over 30s — retrying", order_id);
        let (exec, st, al) = (Arc::clone(executor), Arc::clone(state), Arc::clone(alerter));
        tokio::spawn(async move {
            placer::confirm_stuck_cancel(&*exec, &st, &al, &config_id, &order_id).await;
        });
    }

    // Check every farmed market's open orders against what the legs track, and
    // cancel any stray ones (placer::reconcile; one check per market at a time).
    let tokens = state.read().await.farmed_tokens();
    for token in tokens {
        let (exec, st, al) = (Arc::clone(executor), Arc::clone(state), Arc::clone(alerter));
        tokio::spawn(async move {
            placer::reconcile(&*exec, &st, &al, &token).await;
        });
    }

    // Every leg, with Polymarket's two-sided rule applied across each market's
    // legs, carried out by the same runner the book-update path uses.
    let actions = pairing::decide(&mut *state.write().await, |_| true, tokio::time::Instant::now());
    runner::execute(actions, executor, state, alerter, nudge).await;
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

#[cfg(test)]
mod load_alert_tests {
    use crate::engine::test_support::leg;
    use rust_decimal_macros::dec;

    /// Each leg loaded paused for an invalid value gets an Activity alert that
    /// names it and says how to fix it; valid legs get none.
    #[test]
    fn each_leg_paused_on_load_gets_an_alert() {
        let mut bad_distance = leg("a", dec!(0.01));
        bad_distance.label = "Le Pen".into();
        bad_distance.token_label = "YES".into();
        bad_distance.distance = dec!(0);
        let mut no_token = bad_distance.clone();
        no_token.token_id = String::new();
        no_token.token_label = "NO".into();
        let mut bad_size = leg("s", dec!(0.01));
        bad_size.order_size = dec!(0);
        let mut bad_depth = leg("d", dec!(0.01));
        bad_depth.min_depth_between = dec!(-1);
        // Edit has no tick size field: remove, not edit.
        let bad_tick = leg("t", dec!(0));
        // The first problem shown is one Edit fixes, but the tick still needs a remove.
        let mut bad_distance_and_tick = leg("dt", dec!(0));
        bad_distance_and_tick.distance = dec!(0);
        let configs =
            [leg("ok", dec!(0.01)), bad_distance, no_token, bad_size, bad_depth, bad_tick, bad_distance_and_tick];
        assert_eq!(
            super::paused_on_load(&configs),
            [
                "Paused on load: Le Pen (YES): distance must be > 0, got 0. Fix it in Edit, or remove it.",
                "Paused on load: Le Pen (NO): token_id is empty. Remove it and add the market again.",
                "Paused on load: leg s (Yes): order_size must be > 0, got 0. Fix it in Edit, or remove it.",
                "Paused on load: leg d (Yes): min_depth_between must be >= 0, got -1. Fix it in Edit, or remove it.",
                "Paused on load: leg t (Yes): tick_size must be > 0, got 0. Remove it and add the market again.",
                "Paused on load: leg dt (Yes): distance must be > 0, got 0. Remove it and add the market again.",
            ]
        );
        assert!(super::paused_on_load(&[leg("ok", dec!(0.01))]).is_empty());
    }
}

#[cfg(all(test, unix))]
// Tests raise SIGTERM and read process limits through libc.
#[allow(unsafe_code)]
mod tests {
    use super::{AppState, OrderStatus, RwLock};
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
        // `listen` installs the SIGTERM handler before returning, so raising it
        // now can't hit the default action (which would kill the test binary).
        let mut shutdown = super::Shutdown::listen();
        assert!(!shutdown.requested());
        // SAFETY: raising a signal in our own process; tokio's handler consumes it.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        tokio::time::timeout(Duration::from_secs(3), shutdown.wait()).await.expect("SIGTERM latches the shutdown");
        // Latched: later waits (the next startup retry, the main wait) see it at once.
        assert!(shutdown.requested());
        tokio::time::timeout(Duration::from_millis(100), shutdown.wait()).await.expect("already requested");
    }

    /// Shutdown waits a bounded time for placements in flight, so their orders
    /// are cancelled with the rest — and never waits longer than the limit.
    #[tokio::test(start_paused = true)]
    async fn shutdown_lets_placements_in_flight_land_but_never_waits_forever() {
        let state = RwLock::new(AppState::new(std::path::PathBuf::from("unused.json")));
        assert_eq!(super::settle_placements(&state, Duration::from_secs(5)).await, 0, "nothing to wait for");
        state
            .write()
            .await
            .order_status
            .insert("a".into(), OrderStatus::Placing { price: rust_decimal_macros::dec!(0.3) });
        // Stuck: gives up at the limit and reports it.
        assert_eq!(super::settle_placements(&state, Duration::from_secs(5)).await, 1);
    }

    /// A leg paused (or expired, removed, auto-paused) while its placement is
    /// out goes back to Idle at once, but the call still gets an order back and
    /// cancels it. Before, shutdown only waited for legs marked Placing: it
    /// moved on at once, and the order could land after the final sweep and be
    /// left resting. It waits for the call itself.
    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_a_placement_whose_leg_was_released_meanwhile() {
        use crate::engine::test_support::{env, leg, Fake, TOKEN};
        use crate::engine::{pairing::Decision, placer, quoter::QuoteAction, runner};
        use rust_decimal_macros::dec;
        use std::sync::Arc;
        let e = env(vec![leg("a", dec!(0.01))]);
        let api = Fake::new(dec!(0.01), dec!(0.01));
        let answer = Arc::new(tokio::sync::Notify::new());
        *api.place_gate.lock().unwrap() = Some(Arc::clone(&answer)); // Polymarket is slow to answer
        let place = Decision {
            config_id: "a".into(),
            action: QuoteAction::Place { price: dec!(0.2) },
            token_id: TOKEN.into(),
            tick_size: dec!(0.01),
        };
        runner::execute(vec![place], &api, &e.state, &e.alerter, &Arc::new(tokio::sync::Notify::new())).await;
        // Paused from the dashboard while the call is out: the leg is released.
        {
            let mut s = e.state.write().await;
            s.configs[0].paused = true;
            assert_eq!(placer::settle_unplaced(&mut s, "a"), None);
            assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle));
            s.stopping = true;
        }

        let state = Arc::clone(&e.state);
        let settle = tokio::spawn(async move { super::settle_placements(&state, Duration::from_secs(5)).await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!settle.is_finished(), "waits for the placement call still out");
        answer.notify_one(); // the order lands
        assert_eq!(settle.await.unwrap(), 0, "done within the limit");
        assert_eq!(*api.placed.lock().unwrap(), vec![dec!(0.2)]);
        assert_eq!(*api.cancelled.lock().unwrap(), vec!["0xorder".to_string()], "the order it got back is cancelled");
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

#[cfg(test)]
mod quote_loop_tests {
    use crate::engine::placer::OrderApi;
    use crate::engine::test_support::{env, leg, Fake, TOKEN};
    use eyre::Result;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{watch, Notify};

    const OTHER_TOKEN: &str = "other-token";

    /// The fake exchange, counting how often each market's open orders are listed.
    struct Counting {
        fake: Arc<Fake>,
        listings: Mutex<HashMap<String, u32>>,
    }

    impl Counting {
        fn listings(&self, token: &str) -> u32 {
            self.listings.lock().unwrap().get(token).copied().unwrap_or(0)
        }
    }

    impl OrderApi for Counting {
        fn sdk_tick(&self, t: &str) -> impl Future<Output = Result<Decimal>> + Send {
            self.fake.sdk_tick(t)
        }
        fn refresh_tick(&self, t: &str) -> impl Future<Output = Result<Decimal>> + Send {
            self.fake.refresh_tick(t)
        }
        fn place_buy(&self, t: &str, price: Decimal, shares: Decimal) -> impl Future<Output = Result<String>> + Send {
            self.fake.place_buy(t, price, shares)
        }
        fn cancel(&self, o: &str) -> impl Future<Output = Result<bool>> + Send {
            self.fake.cancel(o)
        }
        fn cancel_verified(&self, o: &str) -> impl Future<Output = Result<bool>> + Send {
            self.fake.cancel_verified(o)
        }
        fn open_orders(&self, t: &str) -> impl Future<Output = Result<Vec<String>>> + Send {
            *self.listings.lock().unwrap().entry(t.to_string()).or_default() += 1;
            self.fake.open_orders(t)
        }
        fn cancel_many(&self, ids: &[String]) -> impl Future<Output = Result<Vec<String>>> + Send {
            self.fake.cancel_many(ids)
        }
    }

    /// A nudge (re-peg confirmed, dashboard resume or edit...) re-evaluates at
    /// once but lists no open orders: the stray-order check of every farmed
    /// market runs on the 30 s timer only, once per market per tick. Before,
    /// each nudge listed every market again, a burst of requests when many
    /// legs re-pegged at once.
    #[tokio::test(start_paused = true)]
    async fn nudges_do_not_relist_open_orders_only_the_timer_does() {
        let mut other = leg("b", dec!(0.01));
        other.token_id = OTHER_TOKEN.into();
        let e = env(vec![leg("a", dec!(0.01)), other]);
        let api = Arc::new(Counting { fake: Fake::new(dec!(0.01), dec!(0.01)), listings: Mutex::new(HashMap::new()) });
        let nudge = Arc::new(Notify::new());
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(super::quote_loop(
            Arc::clone(&e.state),
            Arc::clone(&api),
            Arc::clone(&e.alerter),
            Arc::clone(&nudge),
            stop_rx,
        ));
        let counts = || [api.listings(TOKEN), api.listings(OTHER_TOKEN)];

        // The timer's first tick is at once: one listing per market.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(counts(), [1, 1], "startup tick checks every market once");

        for _ in 0..5 {
            nudge.notify_one();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(counts(), [1, 1], "nudges list no open orders");

        // The next tick checks every market again, once.
        tokio::time::sleep(super::QUOTE_TIMER_INTERVAL).await;
        assert_eq!(counts(), [2, 2], "each timer tick checks every market once");

        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap();
    }
}
