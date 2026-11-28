//! `/events` — the dashboard's single Server-Sent Events stream:
//!   * `alert` — every engine alert, as JSON (feed row + toast);
//!   * `state` — "something the UI shows just changed" (markets, order
//!     statuses, engine phase, connection health). Pages re-fetch the affected
//!     fragments on it instead of blind fixed-interval polling.
//!
//! Every live stream (this one and the market page's book) ends once its
//! session stops being signed in: see [`while_signed_in`].

use std::collections::hash_map::DefaultHasher;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use futures_util::StreamExt;
use tokio::sync::broadcast::error::RecvError;
use tower_sessions::session::Id;
use tower_sessions::Session;

use super::activity::alert_payload;
use super::auth::is_authenticated;
use super::state::WebState;

/// How often the watcher fingerprints engine state for changes.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// How often a stream with nothing to send re-checks its session: a signed-out
/// stream ends within this long even when idle.
const SESSION_RECHECK: Duration = Duration::from_secs(10);

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

/// `events`, ended once `session` is no longer signed in (logged out, signed
/// out by a password change, expired): checked before each event and every
/// [`SESSION_RECHECK`] while idle. The auth guard checks only at connect.
pub(super) fn while_signed_in<S>(state: WebState, session: &Session, events: S) -> impl Stream<Item = S::Item>
where
    S: Stream + Send + 'static,
{
    let id = session.id();
    let recheck = tokio::time::interval_at(tokio::time::Instant::now() + SESSION_RECHECK, SESSION_RECHECK);
    futures_util::stream::unfold(
        (Box::pin(events), recheck, state),
        move |(mut events, mut recheck, state)| async move {
            loop {
                tokio::select! {
                    item = events.next() => {
                        let item = item?;
                        if !signed_in(&state, id).await {
                            return None;
                        }
                        return Some((item, (events, recheck, state)));
                    }
                    _ = recheck.tick() => {
                        if !signed_in(&state, id).await {
                            return None;
                        }
                    }
                }
            }
        },
    )
}

/// Whether session `id` is signed in now, read from the store through a fresh
/// `Session`: the request's own keeps the record it loaded at connect, so it
/// would never see a logout (which deletes the record from the store).
async fn signed_in(state: &WebState, id: Option<Id>) -> bool {
    let current = Session::new(id, Arc::new(state.sessions.clone()), None);
    is_authenticated(&current, state).await
}

/// GET /events
pub async fn stream(
    State(state): State<WebState>,
    session: Session,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
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
    Sse::new(while_signed_in(state, &session, stream)).keep_alive(KeepAlive::default())
}
