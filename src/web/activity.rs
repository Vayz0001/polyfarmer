//! Activity feed: the dashboard's event log. Reads history from `alerts.json`;
//! new events arrive live over `/events` (see `events.rs`).

use askama::Template;
use axum::extract::State;
use axum::response::Html;
use tower_sessions::Session;

use crate::storage::read_recent_alerts;
use crate::types::{Alert, AlertLevel};

use super::shell::{render, shell, Shell};
use super::state::WebState;

/// How many recent events the feed loads on page open.
const FEED_LIMIT: usize = 300;
/// How many the floating Alerts window loads.
const WINDOW_LIMIT: usize = 100;

pub(super) fn level_str(level: &AlertLevel) -> &'static str {
    match level {
        AlertLevel::Info => "info",
        AlertLevel::Warn => "warn",
        AlertLevel::Error => "error",
    }
}

/// A coarse category from the message's first line (its title), for a
/// scannable tag + filtering. Messages are all ours and stable.
pub(super) fn category_for(msg: &str) -> &'static str {
    let m = msg.lines().next().unwrap_or("").to_lowercase();
    // Connection first: "WS disconnected — cancelling 3 open orders" is about
    // the connection, not an individual order.
    if m.starts_with("ws ") || m.contains("disconnect") || m.contains("heartbeat") {
        "connection"
    } else if m.starts_with("hourly summary") {
        "engine"
    } else if m.contains("order") || m.contains("fill") {
        "order"
    } else if m.contains("wallet") || m.contains("auth") {
        "wallet"
    } else if m.contains("pause") || m.contains("expire") || m.contains("market") || m.contains("volatil") {
        "market"
    } else if m.contains("bot ")
        || m.contains("started")
        || m.contains("shutting")
        || m.contains("engine")
        || m.contains("summary")
    {
        "engine"
    } else {
        "event"
    }
}

/// Semantic colour tone, separate from severity level so events that are all
/// `Info` still read differently: a pause/cancel (caution/amber) vs a
/// placement/resume (positive/green). Errors are always red.
pub(super) fn tone_for(level: &AlertLevel, msg: &str) -> &'static str {
    match level {
        AlertLevel::Error => "err",
        AlertLevel::Warn => "caution",
        AlertLevel::Info => {
            let m = msg.lines().next().unwrap_or("").to_lowercase();
            if m.contains("pause")
                || m.contains("cancel")
                || m.contains("expire")
                || m.contains("disconnect")
                || m.contains("shutting")
                || m.contains("stop")
            {
                "caution"
            } else {
                "pos"
            }
        }
    }
}

/// JSON payload for one live alert (`/events` → `alert`).
pub(super) fn alert_payload(alert: &Alert) -> serde_json::Value {
    serde_json::json!({
        "level": level_str(&alert.level),
        "tone": tone_for(&alert.level, &alert.message),
        "category": category_for(&alert.message),
        "message": alert.message,
        "ts": alert.ts.to_rfc3339(),
    })
}

/// Day bucket label for a timestamp, relative to now (UTC).
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

pub struct AlertView {
    pub time_hm: String, // "14:32" fallback if JS is off
    pub ts_iso: String,  // rfc3339 for client relative-time + hover title
    pub level: &'static str,
    pub tone: &'static str, // pos | caution | err — drives colour
    pub category: &'static str,
    pub title: String,
    pub body: String,
}

pub struct DayGroup {
    pub label: String,
    pub items: Vec<AlertView>,
}

pub(super) fn alert_view(a: Alert) -> AlertView {
    let mut lines = a.message.splitn(2, '\n');
    let title = lines.next().unwrap_or("").to_string();
    let body = lines.next().unwrap_or("").to_string();
    AlertView {
        time_hm: a.ts.format("%H:%M").to_string(),
        ts_iso: a.ts.to_rfc3339(),
        level: level_str(&a.level),
        tone: tone_for(&a.level, &a.message),
        category: category_for(&a.message),
        title,
        body,
    }
}

fn group_by_day(alerts: Vec<Alert>) -> Vec<DayGroup> {
    let mut groups: Vec<DayGroup> = Vec::new();
    for a in alerts {
        let label = day_label(&a.ts);
        let view = alert_view(a);
        match groups.last_mut() {
            Some(g) if g.label == label => g.items.push(view),
            _ => groups.push(DayGroup { label, items: vec![view] }),
        }
    }
    groups
}

/// The `n` most recent alerts as flat views (Overview's "recent alerts").
pub(super) fn recent_views(state: &WebState, n: usize) -> Vec<AlertView> {
    read_recent_alerts(&state.alerts_file, n).into_iter().map(alert_view).collect()
}

#[derive(Template)]
#[template(path = "activity.html")]
struct ActivityTemplate {
    shell: Shell,
    groups: Vec<DayGroup>,
    has_any: bool,
}

/// Just the feed rows — for the floating Alerts window's initial load.
#[derive(Template)]
#[template(path = "_activity_feed.html")]
struct ActivityFeedTemplate {
    groups: Vec<DayGroup>,
    has_any: bool,
}

/// GET /activity/recent — rows for the floating Alerts window (live events
/// are then pushed over `/events`).
pub async fn recent(State(state): State<WebState>) -> Html<String> {
    let groups = group_by_day(read_recent_alerts(&state.alerts_file, WINDOW_LIMIT));
    render(&ActivityFeedTemplate { has_any: !groups.is_empty(), groups })
}

/// GET /activity — the full event-log timeline.
pub async fn page(State(state): State<WebState>, session: Session) -> Html<String> {
    let groups = group_by_day(read_recent_alerts(&state.alerts_file, FEED_LIMIT));
    render(&ActivityTemplate { shell: shell(&session, "activity").await, has_any: !groups.is_empty(), groups })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_follow_the_title_line() {
        assert_eq!(category_for("Order placed · Will X?\nBUY 100 Yes shares @ 45¢ ($45)"), "order");
        assert_eq!(
            category_for("Order cancelled · Will X?\n$45 of Yes — depth ahead fell below your minimum"),
            "order"
        );
        assert_eq!(category_for("Auto-paused · Will X?\nBest bid moved"), "market");
        assert_eq!(category_for("Expired · Will X?\nExpiry reached — stopped quoting"), "market");
        assert_eq!(category_for("WS connected"), "connection");
        assert_eq!(category_for("Heartbeat failed 3 consecutive times — cancelling orders and pausing"), "connection");
        assert_eq!(category_for("WS disconnected (stream ended) — cancelling 3 open orders"), "connection");
        assert_eq!(category_for("Bot started"), "engine");
        assert_eq!(category_for("Hourly summary: 3 active markets, 3 live orders"), "engine");
    }

    #[test]
    fn pause_and_expiry_read_as_caution() {
        assert_eq!(tone_for(&AlertLevel::Info, "Paused · X\nOrder cancelled"), "caution");
        assert_eq!(tone_for(&AlertLevel::Info, "Order placed · X\nBUY"), "pos");
    }
}
