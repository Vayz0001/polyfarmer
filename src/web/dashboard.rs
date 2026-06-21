//! Overview + Markets pages — read-only views over the engine's live
//! [`AppState`]. View-models are built here (not in templates) so Askama
//! stays simple: plain strings, no Decimal/enum formatting in markup.

use askama::Template;
use axum::extract::State;
use axum::response::Html;

use crate::types::OrderStatus;

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

    let tpl = OverviewTemplate {
        has_wallet: state.store.has_wallet(),
        engine_running: state.store.has_wallet(),
        active_markets,
        total_markets,
        live_orders,
        paused_markets,
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

/// One row in the markets table, pre-formatted for the template.
pub struct MarketRow {
    pub label: String,
    pub url: String,
    pub side: &'static str,      // "yes" | "no" — css class
    pub side_label: String,      // "YES" | "NO"
    pub distance: String,
    pub size: String,
    pub status_label: String,
    pub status_class: &'static str, // "live" | "idle" | "paused"
    pub price: Option<String>,
}

#[derive(Template)]
#[template(path = "markets.html")]
struct MarketsTemplate {
    has_wallet: bool,
    rows: Vec<MarketRow>,
}

pub async fn markets(State(state): State<WebState>) -> Html<String> {
    let rows = build_rows(&state).await;
    let tpl = MarketsTemplate { has_wallet: state.store.has_wallet(), rows };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

#[derive(Template)]
#[template(path = "_markets_table.html")]
struct MarketsTableTemplate {
    rows: Vec<MarketRow>,
}

/// HTMX polling target: just the table body, re-rendered every 2s.
pub async fn markets_table(State(state): State<WebState>) -> Html<String> {
    let rows = build_rows(&state).await;
    let tpl = MarketsTableTemplate { rows };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

async fn build_rows(state: &WebState) -> Vec<MarketRow> {
    let s = state.engine.read().await;
    s.configs
        .iter()
        .map(|c| {
            let status = s.order_status.get(&c.id).cloned().unwrap_or_default();
            let (status_label, status_class, price) = if c.paused {
                ("Paused".to_string(), "paused", None)
            } else {
                match status {
                    OrderStatus::Live { price, .. } => (
                        "Live".to_string(),
                        "live",
                        Some(format!("{:.2}¢", price * rust_decimal_macros::dec!(100))),
                    ),
                    OrderStatus::Placing { .. } => ("Placing…".to_string(), "idle", None),
                    OrderStatus::Cancelling { .. } => ("Cancelling…".to_string(), "idle", None),
                    OrderStatus::Idle => ("Idle".to_string(), "idle", None),
                }
            };
            MarketRow {
                label: c.label.clone(),
                url: c.url.clone(),
                side: if c.token_label.eq_ignore_ascii_case("yes") { "yes" } else { "no" },
                side_label: c.token_label.clone(),
                distance: format!("{:.0}¢", c.distance * rust_decimal_macros::dec!(100)),
                size: format!("${}", c.order_size),
                status_label,
                status_class,
                price,
            }
        })
        .collect()
}
