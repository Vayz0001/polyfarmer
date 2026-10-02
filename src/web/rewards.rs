//! Rewards: real, Polymarket-reported earning %/$ per market for today
//! (lazy — the underlying call is slow), plus the self-built daily history log
//! (`rewards::history`) as a 30-day chart and table, all on one page.

use std::time::Duration;

use askama::Template;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{NaiveDate, Utc};
use polymarket_client_sdk_v2::clob::types::response::UserRewardsEarningResponse;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tower_sessions::Session;

use crate::storage::load_reward_history;
use crate::types::RewardHistoryFile;

use super::shell::{render, shell, Shell};
use super::state::WebState;

const NETWORK_TIMEOUT: Duration = Duration::from_secs(8);
/// `user_earnings_and_markets_config` is the heaviest call here — it sorts/
/// scores across every reward-eligible market server-side (the SDK's request
/// type can't scope it to "my markets" — see
/// `Executor::user_earnings_and_markets_config`) — so it gets a longer budget.
const MARKETS_CONFIG_TIMEOUT: Duration = Duration::from_secs(20);

// ── History series + chart (shared with Overview) ───────────────────────────

/// The last `days` UTC days (oldest first, ending yesterday — today isn't
/// final until after midnight UTC), with 0 for days without a snapshot.
pub(super) fn history_series(history: &RewardHistoryFile, days: i64) -> Vec<(NaiveDate, Decimal)> {
    let yesterday = Utc::now().date_naive() - chrono::Duration::days(1);
    (0..days)
        .rev()
        .map(|i| {
            let d = yesterday - chrono::Duration::days(i);
            let v = history.snapshots.iter().find(|s| s.date == d).map(|s| s.total_earnings).unwrap_or_default();
            (d, v)
        })
        .collect()
}

pub(super) fn sum_last_days(series: &[(NaiveDate, Decimal)], days: usize) -> Decimal {
    series.iter().rev().take(days).map(|(_, v)| *v).sum()
}

/// Daily rewards as a bar chart — server-rendered SVG (no JS/chart lib).
/// Each bar carries a `<title>` for a native hover tooltip.
pub(super) fn rewards_chart_svg(series: &[(NaiveDate, Decimal)]) -> String {
    const W: f64 = 600.0;
    const H: f64 = 120.0;
    if series.iter().all(|(_, v)| *v <= dec!(0)) {
        return format!(
            "<svg viewBox=\"0 0 {W} {H}\" class=\"bars-svg\" preserveAspectRatio=\"none\" role=\"img\">\
             <text x=\"{}\" y=\"{}\" class=\"chart-empty\" text-anchor=\"middle\">No reward history yet — the first day lands after midnight UTC</text></svg>",
            W / 2.0, H / 2.0
        );
    }
    let max = series.iter().map(|(_, v)| v.to_f64().unwrap_or(0.0)).fold(0.0_f64, f64::max).max(0.0001);
    let n = series.len() as f64;
    let slot = W / n;
    let bar_w = (slot * 0.68).max(1.0);
    let bars: String = series
        .iter()
        .enumerate()
        .map(|(i, (d, v))| {
            let f = v.to_f64().unwrap_or(0.0);
            let h = ((f / max) * (H - 6.0)).max(if f > 0.0 { 2.0 } else { 0.0 });
            let x = i as f64 * slot + (slot - bar_w) / 2.0;
            format!(
                "<rect x=\"{x:.1}\" y=\"{:.1}\" width=\"{bar_w:.1}\" height=\"{h:.1}\" rx=\"1.5\" class=\"bar\"><title>{} · ${:.2}</title></rect>",
                H - h,
                d.format("%b %-d"),
                v
            )
        })
        .collect();
    format!("<svg viewBox=\"0 0 {W} {H}\" class=\"bars-svg\" preserveAspectRatio=\"none\" role=\"img\">{bars}</svg>")
}

// ── Page ─────────────────────────────────────────────────────────────────────

pub struct HistoryRow {
    pub date: String,
    pub total: String,
}

#[derive(Template)]
#[template(path = "rewards.html")]
struct RewardsTemplate {
    shell: Shell,
    chart_svg: String,
    total_7d: String,
    total_30d: String,
    best_day: String,
    rows: Vec<HistoryRow>,
    history_error: Option<String>,
}

pub async fn page(State(state): State<WebState>, session: Session) -> Html<String> {
    let (history, history_error) = match load_reward_history(&state.reward_history_file) {
        Ok(h) => (h, None),
        Err(e) => (RewardHistoryFile::default(), Some(format!("Could not read reward history: {e}"))),
    };
    let series = history_series(&history, 30);
    let best = series.iter().map(|(_, v)| *v).max().unwrap_or_default();
    let mut snaps = history.snapshots.clone();
    snaps.sort_by_key(|s| std::cmp::Reverse(s.date));
    render(&RewardsTemplate {
        shell: shell(&session, "rewards").await,
        chart_svg: rewards_chart_svg(&series),
        total_7d: format!("${:.2}", sum_last_days(&series, 7)),
        total_30d: format!("${:.2}", sum_last_days(&series, 30)),
        best_day: format!("${best:.2}"),
        rows: snaps
            .into_iter()
            .map(|s| HistoryRow { date: s.date.format("%a %b %-d, %Y").to_string(), total: format!("${:.4}", s.total_earnings) })
            .collect(),
        history_error,
    })
}

/// The old standalone history page now lives on /rewards.
pub async fn history_page() -> Response {
    Redirect::to("/rewards").into_response()
}

// ── Today (lazy fragment) ───────────────────────────────────────────────────

pub struct RewardRow {
    pub label: String,
    pub percentage: String,
    pub earnings_today: String,
    pub min_size: String,
    pub max_spread: String,
    pub tracked: bool,
}

#[derive(Template)]
#[template(path = "_rewards_table.html")]
struct RewardsTableTemplate {
    engine_running: bool,
    rows: Vec<RewardRow>,
    today_total: Option<String>,
    error: Option<String>,
}

/// GET /rewards/table — today's per-market share + earnings.
pub async fn table(State(state): State<WebState>) -> Html<String> {
    let Some((executor, _)) = state.engine_handle.get().await else {
        return render(&RewardsTableTemplate { engine_running: false, rows: Vec::new(), today_total: None, error: None });
    };
    let tracked: Vec<String> = state.engine.read().await.configs.iter().map(|c| c.condition_id.to_lowercase()).collect();
    let today = Utc::now().date_naive();

    let rows_fut = tokio::time::timeout(MARKETS_CONFIG_TIMEOUT, executor.user_earnings_and_markets_config(today));
    let total_fut = tokio::time::timeout(NETWORK_TIMEOUT, executor.total_earnings_for_user_for_day(today));
    let (rows_res, total_res) = tokio::join!(rows_fut, total_fut);

    let rows = match rows_res {
        Ok(Ok(entries)) => entries
            .into_iter()
            // The call can't be scoped to "my markets" server-side — only
            // show markets with an actual stake.
            .filter(|e| e.earning_percentage > Decimal::ZERO || !e.earnings.is_empty())
            .map(|e| to_reward_row(e, &tracked))
            .collect(),
        Ok(Err(e)) => {
            return render(&RewardsTableTemplate {
                engine_running: true,
                rows: Vec::new(),
                today_total: None,
                error: Some(format!("Could not load reward data: {e}")),
            })
        }
        Err(_) => {
            return render(&RewardsTableTemplate {
                engine_running: true,
                rows: Vec::new(),
                today_total: None,
                error: Some("Polymarket took too long to answer — it retries on the next refresh.".to_string()),
            })
        }
    };
    // Non-fatal: today's total may legitimately not be ready yet.
    let today_total = match total_res {
        Ok(Ok(entries)) => Some(format!("${:.4}", entries.iter().map(|e| e.earnings).sum::<Decimal>())),
        _ => None,
    };
    render(&RewardsTableTemplate { engine_running: true, rows, today_total, error: None })
}

fn to_reward_row(e: UserRewardsEarningResponse, tracked: &[String]) -> RewardRow {
    let earnings_today: Decimal = e.earnings.iter().map(|a| a.earnings).sum();
    let cid = e.condition_id.to_string().to_lowercase();
    RewardRow {
        label: e.question,
        percentage: format!("{:.2}%", e.earning_percentage),
        earnings_today: format!("${earnings_today:.4}"),
        min_size: format!("{} sh", e.rewards_min_size.normalize()),
        max_spread: format!("{}¢", e.rewards_max_spread.normalize()),
        tracked: tracked.contains(&cid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RewardSnapshot;

    #[test]
    fn series_fills_missing_days_and_sums() {
        let yesterday = Utc::now().date_naive() - chrono::Duration::days(1);
        let h = RewardHistoryFile {
            snapshots: vec![
                RewardSnapshot { date: yesterday, total_earnings: dec!(2), captured_at: Utc::now() },
                RewardSnapshot { date: yesterday - chrono::Duration::days(10), total_earnings: dec!(5), captured_at: Utc::now() },
            ],
        };
        let s = history_series(&h, 30);
        assert_eq!(s.len(), 30);
        assert_eq!(s.last().unwrap(), &(yesterday, dec!(2)));
        assert_eq!(sum_last_days(&s, 7), dec!(2));
        assert_eq!(sum_last_days(&s, 30), dec!(7));
        assert!(rewards_chart_svg(&s).contains("<rect"));
    }
}
