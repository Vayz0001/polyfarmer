//! `/events` — the dashboard's single Server-Sent Events stream:
//!   * `alert` — every engine alert, as JSON (feed row + toast);
//!   * `state` — "something the UI shows just changed" (markets, order
//!     statuses, engine phase, connection health). Pages re-fetch the affected
//!     fragments on it instead of blind fixed-interval polling.

use std::collections::hash_map::DefaultHasher;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use tokio::sync::broadcast::error::RecvError;

use super::activity::alert_payload;
use super::state::WebState;

/// How often the watcher fingerprints engine state for changes.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// Watch engine state and ping `state_tx` whenever its UI-visible parts
/// change. Cheap: one read lock + a hash per second. Call once at startup.
pub fn spawn_state_watcher(state: WebState) {
    tokio::spawn(async move {
        let mut last: u64 = 0;
        let mut ticker = tokio::time::interval(WATCH_INTERVAL);
        loop {
            ticker.tick().await;
            let fp = fingerprint(&state).await;
            if fp != last {
                last = fp;
                let _ = state.state_tx.send(());
            }
        }
    });
}

async fn fingerprint(state: &WebState) -> u64 {
    let s = state.engine.read().await;
    let mut h = DefaultHasher::new();
    for c in &s.configs {
        (&c.id, c.paused, c.order_size, c.distance, c.min_depth_between, c.expires_at, c.max_volatility).hash(&mut h);
    }
    let mut statuses: Vec<String> = s.order_status.iter().map(|(k, v)| format!("{k}{v:?}")).collect();
    statuses.sort_unstable();
    statuses.hash(&mut h);
    format!("{:?}", s.engine_phase).hash(&mut h);
    (s.ws_connected, s.heartbeat_paused, s.last_heartbeat_ok.is_some()).hash(&mut h);
    h.finish()
}

/// GET /events
pub async fn stream(State(state): State<WebState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let alerts = state.alert_tx.subscribe();
    let changes = state.state_tx.subscribe();
    let stream = futures_util::stream::unfold((alerts, changes), |(mut alerts, mut changes)| async move {
        loop {
            tokio::select! {
                r = alerts.recv() => match r {
                    Ok(alert) => {
                        let ev = Event::default().event("alert").data(alert_payload(&alert).to_string());
                        return Some((Ok(ev), (alerts, changes)));
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                },
                r = changes.recv() => match r {
                    Ok(()) | Err(RecvError::Lagged(_)) => {
                        return Some((Ok(Event::default().event("state").data("1")), (alerts, changes)));
                    }
                    Err(RecvError::Closed) => return None,
                },
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
