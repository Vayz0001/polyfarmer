//! Overview + Markets pages — read-only views over the engine's live
//! [`AppState`]. View-models are built here (not in templates) so Askama
//! stays simple: plain strings, no Decimal/enum formatting in markup.

use std::collections::HashMap;

use askama::Template;
use axum::extract::State;
use axum::response::Html;
use tower_sessions::Session;

use crate::types::OrderStatus;

use super::auth::csrf_token;
use super::state::WebState;

#[derive(Template)]
#[template(path = "index.html")]
struct OverviewTemplate {
    has_wallet: bool,
    engine_running: bool,
    active_markets: usize,
    total_markets: usize,
    live_orders: usize,
    paused_markets: usize,
    /// Real total from Polymarket (`total_earnings_for_user_for_day`), not an
    /// estimate — `None` if the engine isn't running or the call failed.
    rewards_today: Option<String>,
}

pub async fn overview(State(state): State<WebState>) -> Html<String> {
    let s = state.engine.read().await;
    let total_markets = s.configs.len();
    let paused_markets = s.configs.iter().filter(|c| c.paused).count();
    let active_markets = total_markets - paused_markets;
    let live_orders = s
        .order_status
        .values()
        .filter(|st| matches!(st, OrderStatus::Live { .. }))
        .count();
    drop(s);

    let rewards_today = match state.engine_handle.get().await {
        Some((executor, _)) => {
            let today = chrono::Utc::now().date_naive();
            match tokio::time::timeout(
                std::time::Duration::from_secs(8),
                executor.total_earnings_for_user_for_day(today),
            )
            .await
            {
                Ok(Ok(entries)) => {
                    let sum: rust_decimal::Decimal = entries.iter().map(|e| e.earnings).sum();
                    Some(format!("${sum:.4}"))
                }
                _ => None,
            }
        }
        None => None,
    };

    let tpl = OverviewTemplate {
        has_wallet: state.store.has_wallet(),
        engine_running: state.store.has_wallet(),
        active_markets,
        total_markets,
        live_orders,
        paused_markets,
        rewards_today,
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

/// One outcome leg within a market row — one side of a one- or both-sides
/// farming config.
pub struct LegRow {
    pub side_label: String, // the market's real outcome name, e.g. "Yes", "France"
    /// "a" | "b" — which outcome slot this is (first/second token), purely
    /// for a stable, outcome-agnostic color split. Never matches on the
    /// literal text "yes"/"no".
    pub side_class: &'static str,
    pub distance: String,
    pub size: String,
    pub status_label: String,
    pub status_class: &'static str, // "live" | "idle" | "paused"
    pub price: Option<String>,
    /// Whether the live order is currently scoring for rewards — `None` when
    /// not Live or the engine isn't running to ask. Real, per-order data from
    /// Polymarket (`is_order_scoring`/`are_orders_scoring`), not a guess.
    pub scoring: Option<bool>,
}

/// One row in the markets table — one *market* (grouped by `condition_id`),
/// holding one leg for one-side farming or two legs for both-sides.
pub struct MarketRow {
    /// `condition_id` — the group key; pause/remove/resume act on every leg
    /// sharing it.
    pub condition_id: String,
    pub label: String,
    pub url: String,
    pub legs: Vec<LegRow>,
    pub paused: bool,
}

#[derive(Template)]
#[template(path = "markets.html")]
struct MarketsTemplate {
    has_wallet: bool,
    csrf_token: String,
    rows: Vec<MarketRow>,
}

pub async fn markets(State(state): State<WebState>, session: Session) -> Html<String> {
    let rows = build_rows(&state).await;
    let tpl = MarketsTemplate {
        has_wallet: state.store.has_wallet(),
        csrf_token: csrf_token(&session).await,
        rows,
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

#[derive(Template)]
#[template(path = "_markets_table.html")]
struct MarketsTableTemplate {
    csrf_token: String,
    rows: Vec<MarketRow>,
}

/// HTMX polling target: just the table body, re-rendered every 2s.
pub async fn markets_table(State(state): State<WebState>, session: Session) -> Html<String> {
    let rows = build_rows(&state).await;
    let tpl = MarketsTableTemplate { csrf_token: csrf_token(&session).await, rows };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

async fn build_rows(state: &WebState) -> Vec<MarketRow> {
    let (configs, order_status) = {
        let s = state.engine.read().await;
        (s.configs.clone(), s.order_status.clone())
    };

    // Batch-check reward-scoring for every currently-Live order in one call
    // instead of one per row.
    let live_order_ids: Vec<String> = order_status
        .values()
        .filter_map(|st| match st {
            OrderStatus::Live { order_id, .. } => Some(order_id.clone()),
            _ => None,
        })
        .collect();
    let scoring: HashMap<String, bool> = if live_order_ids.is_empty() {
        HashMap::new()
    } else if let Some((executor, _)) = state.engine_handle.get().await {
        let refs: Vec<&str> = live_order_ids.iter().map(String::as_str).collect();
        executor.are_orders_scoring(&refs).await.unwrap_or_default()
    } else {
        HashMap::new()
    };

    // Group configs by condition_id, preserving first-seen order, so a
    // both-sides farm (2 configs, 1 condition_id) renders as one row.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&crate::types::MarketConfig>> = HashMap::new();
    for c in &configs {
        let entry = groups.entry(c.condition_id.clone()).or_default();
        if entry.is_empty() {
            order.push(c.condition_id.clone());
        }
        entry.push(c);
    }

    order
        .into_iter()
        .map(|cid| {
            let legs_cfg = groups.remove(&cid).unwrap_or_default();
            let label = legs_cfg.first().map(|c| c.label.clone()).unwrap_or_default();
            let url = legs_cfg.first().map(|c| c.url.clone()).unwrap_or_default();
            let paused = legs_cfg.first().is_some_and(|c| c.paused);
            let legs = legs_cfg
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let status = order_status.get(&c.id).cloned().unwrap_or_default();
                    let (status_label, status_class, price, is_scoring) = if c.paused {
                        ("Paused".to_string(), "paused", None, None)
                    } else {
                        match &status {
                            OrderStatus::Live { order_id, price } => (
                                "Live".to_string(),
                                "live",
                                Some(format!("{:.2}¢", price * rust_decimal_macros::dec!(100))),
                                scoring.get(order_id).copied(),
                            ),
                            OrderStatus::Placing { .. } => {
                                ("Placing…".to_string(), "idle", None, None)
                            }
                            OrderStatus::Cancelling { .. } => {
                                ("Cancelling…".to_string(), "idle", None, None)
                            }
                            OrderStatus::Idle => ("Idle".to_string(), "idle", None, None),
                        }
                    };
                    LegRow {
                        side_label: c.token_label.clone(),
                        side_class: if i == 0 { "a" } else { "b" },
                        distance: format!("{:.0}¢", c.distance * rust_decimal_macros::dec!(100)),
                        size: format!("${}", c.order_size),
                        status_label,
                        status_class,
                        price,
                        scoring: is_scoring,
                    }
                })
                .collect();
            MarketRow { condition_id: cid, label, url, legs, paused }
        })
        .collect()
}
