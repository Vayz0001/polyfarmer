use crate::engine::alerts::Alerter;
use crate::engine::executor::Executor;
use crate::engine::ws_manager::AppState;
use crate::types::OrderStatus;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, RwLock};
use tracing::{error, info, warn};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

pub fn spawn(
    executor: Arc<Executor>,
    alerter: Arc<Alerter>,
    state: Arc<RwLock<AppState>>,
    mut stop_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        let mut failures: u32 = 0;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match executor.send_heartbeat().await {
                        Ok(_) => {
                            if failures > 0 {
                                info!("Heartbeat recovered after {} failures", failures);
                            }
                            failures = 0;
                            state.write().await.last_heartbeat_ok = Some(std::time::Instant::now());

                            // Clear pause flag if heartbeat recovers
                            let was_paused = {
                                let s = state.read().await;
                                s.heartbeat_paused
                            };
                            if was_paused {
                                let mut s = state.write().await;
                                s.heartbeat_paused = false;
                                drop(s);
                                alerter.info("Heartbeat recovered — resuming order placement");
                            }
                        }
                        Err(e) => {
                            failures += 1;
                            warn!("Heartbeat failed ({}/{}): {}", failures, MAX_CONSECUTIVE_FAILURES, e);

                            if failures >= MAX_CONSECUTIVE_FAILURES {
                                error!("Heartbeat failed {} times — cancelling bot orders and pausing placement", failures);
                                alerter.error(format!(
                                    "Heartbeat failed {} consecutive times — cancelling orders and pausing",
                                    failures
                                ));

                                // Include Cancelling — the in-flight cancel may not have
                                // completed before the CLOB became unreachable; re-cancel is idempotent.
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

                                match executor.cancel_orders(&live_order_ids).await {
                                    Ok(_) => {
                                        // Reset Live statuses to Idle and pause new placements.
                                        // Orders are cancelled on exchange; state must reflect that.
                                        let mut s = state.write().await;
                                        for status in s.order_status.values_mut() {
                                            if matches!(status, OrderStatus::Live { .. }) {
                                                *status = OrderStatus::Idle;
                                            }
                                        }
                                        s.heartbeat_paused = true;
                                        drop(s);
                                        alerter.warn("Orders cancelled — placement paused until heartbeat recovers");
                                    }
                                    Err(e) => {
                                        error!("Heartbeat cancel_orders failed: {}", e);
                                        alerter.error(format!(
                                            "Heartbeat cancel failed — manual check required: {}",
                                            e
                                        ));
                                        // Still pause placement even if cancel failed — CLOB unreachable
                                        let mut s = state.write().await;
                                        s.heartbeat_paused = true;
                                    }
                                }
                                failures = 0;
                            }
                        }
                    }
                }
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        info!("Heartbeat task stopping");
                        break;
                    }
                }
            }
        }
    });
}
