//! "My Rewards": real, Polymarket-reported earning %/$ per tracked market
//! (not estimates — see `Executor::user_earnings_and_markets_config`), plus
//! the self-built daily history log (`rewards::history`).

use std::time::Duration;

use askama::Template;
use axum::extract::State;
use axum::response::Html;
use chrono::Utc;
use polymarket_client_sdk_v2::clob::types::response::UserRewardsEarningResponse;
use rust_decimal::Decimal;

use crate::storage::load_reward_history;

use super::state::WebState;

const NETWORK_TIMEOUT: Duration = Duration::from_secs(8);
/// `user_earnings_and_markets_config` is the heaviest call here — it sorts/
/// scores across every reward-eligible market on the platform server-side
/// (the SDK's request type can't scope it to "my markets" — see
/// `Executor::user_earnings_and_markets_config`) — observed meaningfully
/// slower than the other reward calls, so it gets a longer budget.
const MARKETS_CONFIG_TIMEOUT: Duration = Duration::from_secs(20);

pub struct RewardRow {
    pub label: String,
    pub percentage: String,
    pub earnings_today: String,
    pub min_size: String,
    pub max_spread: String,
}

struct RewardsData {
    engine_running: bool,
    rows: Vec<RewardRow>,
    today_total: Option<String>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "rewards.html")]
struct RewardsTemplate {
    engine_running: bool,
    rows: Vec<RewardRow>,
    today_total: Option<String>,
    error: Option<String>,
}

pub async fn page(State(state): State<WebState>) -> Html<String> {
    let d = fetch(&state).await;
    let tpl = RewardsTemplate {
        engine_running: d.engine_running,
        rows: d.rows,
        today_total: d.today_total,
        error: d.error,
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

#[derive(Template)]
#[template(path = "_rewards_table.html")]
struct RewardsTableTemplate {
    engine_running: bool,
    rows: Vec<RewardRow>,
    today_total: Option<String>,
    error: Option<String>,
}

/// htmx-polled partial (~10-15s — a live % share doesn't need 2s freshness
/// the way order status does).
pub async fn table(State(state): State<WebState>) -> Html<String> {
    let d = fetch(&state).await;
    let tpl = RewardsTableTemplate {
        engine_running: d.engine_running,
        rows: d.rows,
        today_total: d.today_total,
        error: d.error,
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

async fn fetch(state: &WebState) -> RewardsData {
    let Some((executor, _)) = state.engine_handle.get().await else {
        return RewardsData { engine_running: false, rows: Vec::new(), today_total: None, error: None };
    };

    let today = Utc::now().date_naive();

    let rows = match tokio::time::timeout(MARKETS_CONFIG_TIMEOUT, executor.user_earnings_and_markets_config(today)).await {
        Ok(Ok(entries)) => entries
            .into_iter()
            // The underlying call can't be scoped to "my markets" server-side
            // (see Executor::user_earnings_and_markets_config) — only show
            // markets where there's an actual stake, not the whole platform
            // list with a 0% row for everything you're not in.
            .filter(|e| e.earning_percentage > Decimal::ZERO || !e.earnings.is_empty())
            .map(to_reward_row)
            .collect(),
        Ok(Err(e)) => {
            return RewardsData {
                engine_running: true,
                rows: Vec::new(),
                today_total: None,
                error: Some(format!("Could not load reward data: {e}")),
            }
        }
        Err(_) => {
            return RewardsData {
                engine_running: true,
                rows: Vec::new(),
                today_total: None,
                error: Some("Request to Polymarket timed out — try again.".to_string()),
            }
        }
    };

    // Non-fatal: today's total may legitimately not be ready/final yet —
    // the per-market table above is the primary view either way.
    let today_total = match tokio::time::timeout(NETWORK_TIMEOUT, executor.total_earnings_for_user_for_day(today)).await
    {
        Ok(Ok(entries)) => {
            let sum: Decimal = entries.iter().map(|e| e.earnings).sum();
            Some(format!("${sum:.4}"))
        }
        _ => None,
    };

    RewardsData { engine_running: true, rows, today_total, error: None }
}

fn to_reward_row(e: UserRewardsEarningResponse) -> RewardRow {
    let earnings_today: Decimal = e.earnings.iter().map(|a| a.earnings).sum();
    RewardRow {
        label: e.question,
        percentage: format!("{:.1}%", e.earning_percentage),
        earnings_today: format!("${earnings_today:.4}"),
        min_size: format!("${}", e.rewards_min_size),
        max_spread: format!("{:.1}c", e.rewards_max_spread),
    }
}

// ── History (self-built daily snapshot — see rewards::history) ─────────────

pub struct HistoryRow {
    pub date: String,
    pub total: String,
}

#[derive(Template)]
#[template(path = "rewards_history.html")]
struct HistoryTemplate {
    rows: Vec<HistoryRow>,
    error: Option<String>,
}

pub async fn history_page(State(state): State<WebState>) -> Html<String> {
    let (rows, error) = load_history_rows(&state);
    let tpl = HistoryTemplate { rows, error };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

#[derive(Template)]
#[template(path = "_reward_history_table.html")]
struct HistoryTableTemplate {
    rows: Vec<HistoryRow>,
    error: Option<String>,
}

pub async fn history_table(State(state): State<WebState>) -> Html<String> {
    let (rows, error) = load_history_rows(&state);
    let tpl = HistoryTableTemplate { rows, error };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

/// Pure local file read (no network) — the daily poller (`rewards::history`)
/// is what does the network call, once a day. Most recent first.
fn load_history_rows(state: &WebState) -> (Vec<HistoryRow>, Option<String>) {
    match load_reward_history(&state.reward_history_file) {
        Ok(mut history) => {
            history.snapshots.sort_by_key(|s| std::cmp::Reverse(s.date));
            let rows = history
                .snapshots
                .into_iter()
                .map(|s| HistoryRow { date: s.date.to_string(), total: format!("${:.4}", s.total_earnings) })
                .collect();
            (rows, None)
        }
        Err(e) => (Vec::new(), Some(format!("Could not read reward history: {e}"))),
    }
}
