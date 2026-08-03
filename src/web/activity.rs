//! Activity feed: the dashboard's event log. Reads history from `alerts.json`
//! and streams new events live over SSE (the engine's `Alerter` fans out to a
//! broadcast channel; see `state::WebState::alert_tx`).

use std::convert::Infallible;

use askama::Template;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use futures_util::stream::Stream;
use tokio::sync::broadcast;

use crate::storage::read_recent_alerts;
use crate::types::{Alert, AlertLevel};
use crate::web::state::WebState;

/// How many recent events the feed loads on page open.
const FEED_LIMIT: usize = 200;

fn level_str(level: &AlertLevel) -> &'static str {
    match level {
        AlertLevel::Info => "info",
        AlertLevel::Warn => "warn",
        AlertLevel::Error => "error",
    }
}

/// A coarse category derived from the message, for a scannable tag + filtering.
/// (Messages are all ours and stable, so keyword matching is reliable; a
/// structured field can replace this later without touching the UI.)
fn category_for(msg: &str) -> &'static str {
    let m = msg.to_lowercase();
    if m.contains("order placed") || m.contains("placed") || m.contains("cancel") || m.contains("replace") || m.contains("fill") {
        "order"
    } else if m.contains("ws ") || m.contains("disconnect") || m.contains("connected") || m.contains("heartbeat") {
        "connection"
    } else if m.contains("wallet") || m.contains("auth") {
        "wallet"
    } else if m.contains("pause") || m.contains("expire") || m.contains("depth") || m.contains("volatil") {
        "market"
    } else if m.contains("started") || m.contains("shutting down") || m.contains("shutdown") || m.contains("engine") {
        "engine"
    } else {
        "event"
    }
}

/// Semantic colour tone, separate from severity level so events that are all
/// `Info` still read differently: a pause/cancel (caution/amber) vs a
/// placement/resume (positive/green). Errors are always red.
fn tone_for(level: &AlertLevel, msg: &str) -> &'static str {
    match level {
        AlertLevel::Error => "err",
        AlertLevel::Warn => "caution",
        AlertLevel::Info => {
            let m = msg.to_lowercase();
            if m.contains("pause") || m.contains("cancel") || m.contains("expire")
                || m.contains("disconnect") || m.contains("shutting") || m.contains("stop")
            {
                "caution"
            } else {
                "pos"
            }
        }
    }
}

/// Day bucket label for a timestamp, relative to now (viewer-agnostic UTC).
fn day_label(ts: &chrono::DateTime<chrono::Utc>) -> String {
    let today = chrono::Utc::now().date_naive();
    let d = ts.date_naive();
    if d == today {
        "Today".to_string()
    } else if d == today.pred_opt().unwrap_or(today) {
        "Yesterday".to_string()
    } else {
        d.format("%b %-d, %Y").to_string()
    }
}

struct AlertView {
    time_hm: String, // "14:32" fallback if JS is off
    ts_iso: String,  // rfc3339 for client relative-time + hover title
    level: &'static str,
    tone: &'static str, // pos | caution | err — drives colour
    category: &'static str,
    message: String, // may contain newlines (rendered white-space: pre-line)
}

struct DayGroup {
    label: String,
    items: Vec<AlertView>,
}

fn group_by_day(alerts: Vec<Alert>) -> Vec<DayGroup> {
    let mut groups: Vec<DayGroup> = Vec::new();
    for a in alerts {
        let label = day_label(&a.ts);
        let view = AlertView {
            time_hm: a.ts.format("%H:%M").to_string(),
            ts_iso: a.ts.to_rfc3339(),
            level: level_str(&a.level),
            tone: tone_for(&a.level, &a.message),
            category: category_for(&a.message),
            message: a.message,
        };
        match groups.last_mut() {
            Some(g) if g.label == label => g.items.push(view),
            _ => groups.push(DayGroup { label, items: vec![view] }),
        }
    }
    groups
}

#[derive(Template)]
#[template(path = "activity.html")]
struct ActivityTemplate {
    engine_running: bool,
    groups: Vec<DayGroup>,
    has_any: bool,
}

/// GET /activity — the full event-log timeline.
pub async fn page(State(state): State<WebState>) -> Html<String> {
    let alerts = read_recent_alerts(&state.alerts_file, FEED_LIMIT);
    let groups = group_by_day(alerts);
    let tpl = ActivityTemplate {
        engine_running: state.store.has_wallet(),
        has_any: !groups.is_empty(),
        groups,
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

/// GET /activity/stream — Server-Sent Events: one `alert` event per new alert.
/// Payload is JSON `{ level, category, message, ts }` the client renders into a
/// feed row + toast.
pub async fn stream(State(state): State<WebState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.alert_tx.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(alert) => {
                    let payload = serde_json::json!({
                        "level": level_str(&alert.level),
                        "tone": tone_for(&alert.level, &alert.message),
                        "category": category_for(&alert.message),
                        "message": alert.message,
                        "ts": alert.ts.to_rfc3339(),
                    });
                    let ev = Event::default().event("alert").data(payload.to_string());
                    return Some((Ok(ev), rx));
                }
                // Client fell behind — skip the gap, keep streaming.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
