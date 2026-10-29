//! Rewards: today's live per-market share and earnings (authenticated, slow
//! call, cached), daily payouts from Polymarket's public activity feed, and
//! your LP rank from PolyRewards. Every panel is a lazy fragment.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::extract::State;
use axum::response::Html;
use chrono::{NaiveDate, Utc};
use polymarket_client_sdk_v2::clob::types::response::UserRewardsEarningResponse;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tower_sessions::Session;

use crate::engine::executor::Executor;
use crate::rewards::lp_rank::{self, LpRank};
use crate::rewards::portfolio::{self, DailyPayout};
use crate::types::{MarketConfig, OrderStatus};

use super::dashboard::slug_of;
use super::shell::{render, shell, Shell};
use super::state::WebState;

/// `user_earnings_and_markets_config` is the heaviest call here — it sorts/
/// scores across every reward-eligible market server-side (the SDK's request
/// type can't scope it to "my markets" — see
/// `Executor::user_earnings_and_markets_config`) — so it gets a longer budget.
const MARKETS_CONFIG_TIMEOUT: Duration = Duration::from_secs(20);

// ── History series + chart (shared with Overview) ───────────────────────────

/// The last `days` UTC days (oldest first, ending yesterday — today isn't
/// paid until after midnight UTC), with 0 for days without a payout.
pub(super) fn history_series(payouts: &[DailyPayout], days: i64) -> Vec<(NaiveDate, Decimal)> {
    let yesterday = Utc::now().date_naive() - chrono::Duration::days(1);
    (0..days)
        .rev()
        .map(|i| {
            let d = yesterday - chrono::Duration::days(i);
            let v = payouts.iter().find(|p| p.earned_day == d).map(|p| p.amount).unwrap_or_default();
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
             <text x=\"{}\" y=\"{}\" class=\"chart-empty\" text-anchor=\"middle\">No payouts in the last 30 days — each day is paid after midnight UTC</text></svg>",
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

// ── Today's earnings (shared) ───────────────────────────────────────────────

/// Fetch today's per-market reward entries. `'static`, so Overview can run it
/// as a background refresh (`get_swr`) without holding up its other numbers.
pub(super) async fn fetch_today_entries(executor: Arc<Executor>) -> eyre::Result<Arc<Vec<UserRewardsEarningResponse>>> {
    let today = Utc::now().date_naive();
    let entries = tokio::time::timeout(MARKETS_CONFIG_TIMEOUT, executor.user_earnings_and_markets_config(today))
        .await
        .map_err(|_| eyre::eyre!("Polymarket took too long to answer — it retries on the next refresh."))??;
    Ok(Arc::new(entries))
}

/// Cache key for today's entries: the UTC date, so totals reset at midnight UTC.
pub(super) fn today_key() -> String {
    Utc::now().date_naive().to_string()
}

/// Today's per-market reward entries, cached (see `Caches::reward_markets`).
/// `None` when the engine isn't running and nothing is cached.
pub(super) async fn today_entries(state: &WebState) -> Option<eyre::Result<Arc<Vec<UserRewardsEarningResponse>>>> {
    match state.engine_handle.get().await {
        Some((executor, _)) => {
            Some(state.caches.reward_markets.get_or_fetch(&today_key(), || fetch_today_entries(executor)).await)
        }
        None => state.caches.reward_markets.peek(&today_key()).map(|(v, _)| Ok(v)),
    }
}

/// What one market has paid so far today.
pub(super) fn earned(e: &UserRewardsEarningResponse) -> Decimal {
    e.earnings.iter().map(|a| a.earnings).sum()
}

/// Today's total across markets: the sum of the per-market amounts, so it always
/// matches the rows. (Polymarket's day-total endpoint lags during the day.)
pub(super) fn total_earned(entries: &[UserRewardsEarningResponse]) -> Decimal {
    entries.iter().map(earned).sum()
}

// ── Payouts (shared with Overview) ──────────────────────────────────────────

const PAYOUTS_TIMEOUT: Duration = Duration::from_secs(30);

/// Daily payouts for the configured wallet, newest first, from Polymarket's
/// public activity feed (cached). `Err` is a message for the page.
pub(super) async fn load_payouts(state: &WebState) -> Result<Vec<DailyPayout>, String> {
    let Some(user) = state.wallet_address() else {
        return Err("Configure your wallet in Settings to see payouts.".into());
    };
    state
        .caches
        .reward_payouts
        .get_or_fetch(&user, || async {
            tokio::time::timeout(PAYOUTS_TIMEOUT, portfolio::reward_payouts(&user))
                .await
                .map_err(|_| eyre::eyre!("Polymarket took too long to answer"))?
        })
        .await
        .map(|p| portfolio::daily_payouts(&p))
        .map_err(|e| format!("Couldn't load payouts: {e}"))
}

/// "1,234,567".
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn money(v: Decimal) -> String {
    format!("${v:.2}")
}

// ── Page ─────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "rewards.html")]
struct RewardsTemplate {
    shell: Shell,
}

/// GET /rewards — the shell; every panel loads as its own fragment.
pub async fn page(session: Session) -> Html<String> {
    render(&RewardsTemplate { shell: shell(&session, "rewards").await })
}

// ── KPI row ──────────────────────────────────────────────────────────────────

pub struct Kpi {
    pub label: &'static str,
    pub value: String,
    pub sub: String,
    /// Green value (money earned).
    pub pos: bool,
}

#[derive(Template)]
#[template(path = "_rewards_kpis.html")]
struct KpisTemplate {
    kpis: Vec<Kpi>,
    rank: RankView,
}

pub struct RankView {
    pub value: String,
    pub tier: Option<String>,
    pub sub: String,
    pub link: &'static str,
}

/// GET /rewards/kpis — today so far, yesterday, last 7 days, all time, LP rank.
pub async fn kpis(State(state): State<WebState>) -> Html<String> {
    let (today, payouts) = tokio::join!(today_entries(&state), load_payouts(&state));

    let today_kpi = match today {
        Some(Ok(entries)) => Kpi {
            label: "Today so far",
            value: money(total_earned(&entries)),
            sub: "since 00:00 UTC".into(),
            pos: true,
        },
        Some(Err(_)) => Kpi {
            label: "Today so far",
            value: "—".into(),
            sub: "Polymarket didn't answer, retrying".into(),
            pos: true,
        },
        None => Kpi { label: "Today so far", value: "—".into(), sub: "engine not running".into(), pos: true },
    };

    let mut kpis = vec![today_kpi];
    match &payouts {
        Ok(days) => {
            let yesterday = Utc::now().date_naive() - chrono::Duration::days(1);
            kpis.push(match days.iter().find(|d| d.earned_day == yesterday) {
                Some(d) => Kpi {
                    label: "Yesterday",
                    value: money(d.amount),
                    sub: format!("paid {}", d.paid_at.format("%b %-d, %H:%M UTC")),
                    pos: true,
                },
                None => Kpi { label: "Yesterday", value: money(Decimal::ZERO), sub: "no payout yet".into(), pos: true },
            });
            let series = history_series(days, 30);
            kpis.push(Kpi {
                label: "Last 7 days",
                value: money(sum_last_days(&series, 7)),
                sub: format!("30 days: {}", money(sum_last_days(&series, 30))),
                pos: true,
            });
            let total: Decimal = days.iter().map(|d| d.amount).sum();
            let sub = match days.last() {
                Some(first) => format!(
                    "{} day{} paid since {}",
                    days.len(),
                    if days.len() == 1 { "" } else { "s" },
                    first.earned_day.format("%b %-d, %Y")
                ),
                None => "no payouts yet".into(),
            };
            kpis.push(Kpi { label: "All time", value: money(total), sub, pos: true });
        }
        Err(e) => {
            for label in ["Yesterday", "Last 7 days", "All time"] {
                kpis.push(Kpi { label, value: "—".into(), sub: e.clone(), pos: true });
            }
        }
    }

    render(&KpisTemplate { kpis, rank: rank_view(&state) })
}

/// The rank card never waits: it shows the cached rank and refreshes it in
/// the background (a download of PolyRewards' public list, ~26 MB).
fn rank_view(state: &WebState) -> RankView {
    let link = lp_rank::SITE_URL;
    let Some(user) = state.wallet_address() else {
        return RankView { value: "—".into(), tier: None, sub: "configure your wallet".into(), link };
    };
    let fetch_user = user.clone();
    let cached = state
        .caches
        .lp_rank
        .get_swr(&user, move || async move { Ok::<_, std::convert::Infallible>(lp_rank::fetch(&fetch_user).await) });
    match cached {
        Some(r @ LpRank::Ranked { rank, total }) => RankView {
            value: format!("#{}", thousands(rank)),
            tier: r.tier(),
            sub: format!("of {} LPs", thousands(total)),
            link,
        },
        Some(LpRank::NotRanked { total }) => {
            RankView { value: "—".into(), tier: None, sub: format!("not among {} LPs yet", thousands(total)), link }
        }
        Some(LpRank::Unavailable) => RankView { value: "—".into(), tier: None, sub: "rank unavailable".into(), link },
        None => RankView { value: "…".into(), tier: None, sub: "checking PolyRewards".into(), link },
    }
}

// ── Payout history (chart + table) ───────────────────────────────────────────

pub struct PayoutRow {
    pub day: String,
    pub paid_at: String,
    pub amount: String,
}

#[derive(Template)]
#[template(path = "_rewards_history.html")]
struct HistoryTemplate {
    error: Option<String>,
    chart_svg: String,
    total_30d: String,
    rows: Vec<PayoutRow>,
}

/// GET /rewards/history — the 30-day chart, with every payout as a table.
pub async fn history(State(state): State<WebState>) -> Html<String> {
    let days = match load_payouts(&state).await {
        Ok(d) => d,
        Err(e) => {
            return render(&HistoryTemplate {
                error: Some(e),
                chart_svg: String::new(),
                total_30d: String::new(),
                rows: Vec::new(),
            })
        }
    };
    let series = history_series(&days, 30);
    render(&HistoryTemplate {
        error: None,
        chart_svg: rewards_chart_svg(&series),
        total_30d: money(sum_last_days(&series, 30)),
        rows: days
            .iter()
            .map(|d| PayoutRow {
                day: d.earned_day.format("%a %b %-d, %Y").to_string(),
                paid_at: d.paid_at.format("%b %-d, %H:%M UTC").to_string(),
                amount: format!("${:.4}", d.amount),
            })
            .collect(),
    })
}

// ── Today, by market ─────────────────────────────────────────────────────────

pub struct RewardRow {
    pub label: String,
    pub slug: String,
    /// "farming", "paused", "waiting" or "removed".
    pub status: &'static str,
    pub share: String,
    pub earned_today: String,
    pub pool_per_day: String,
    earned: Decimal,
    share_pct: Decimal,
}

#[derive(Template)]
#[template(path = "_rewards_table.html")]
struct RewardsTableTemplate {
    engine_running: bool,
    rows: Vec<RewardRow>,
    today_total: String,
    error: Option<String>,
}

/// GET /rewards/table — every market this bot farms, plus any other market that
/// paid you today (e.g. one you removed), with your live share and earnings.
pub async fn table(State(state): State<WebState>) -> Html<String> {
    let entries = match today_entries(&state).await {
        Some(Ok(entries)) => entries,
        Some(Err(e)) => {
            return render(&RewardsTableTemplate {
                engine_running: true,
                rows: Vec::new(),
                today_total: String::new(),
                error: Some(format!("Could not load reward data: {e}")),
            })
        }
        None => {
            return render(&RewardsTableTemplate {
                engine_running: false,
                rows: Vec::new(),
                today_total: String::new(),
                error: None,
            })
        }
    };
    let (configs, order_status) = {
        let s = state.engine.read().await;
        (s.configs.clone(), s.order_status.clone())
    };
    let rows = build_table(&entries, &configs, &order_status);
    render(&RewardsTableTemplate {
        engine_running: true,
        rows,
        today_total: money(total_earned(&entries)),
        error: None,
    })
}

/// Farmed markets first (with or without a share yet), then other markets
/// that paid today; markets that are neither are left out. Highest earner first.
fn build_table(
    entries: &[UserRewardsEarningResponse],
    configs: &[MarketConfig],
    order_status: &HashMap<String, OrderStatus>,
) -> Vec<RewardRow> {
    let status_of = |cid: &str| -> &'static str {
        let legs: Vec<&MarketConfig> = configs.iter().filter(|c| c.condition_id.eq_ignore_ascii_case(cid)).collect();
        if legs.is_empty() {
            "removed"
        } else if legs.iter().any(|c| !c.paused && matches!(order_status.get(&c.id), Some(OrderStatus::Live { .. }))) {
            "farming"
        } else if legs.iter().all(|c| c.paused) {
            "paused"
        } else {
            "waiting"
        }
    };

    let mut rows: Vec<RewardRow> = entries
        .iter()
        .filter_map(|e| {
            let cid = e.condition_id.to_string();
            let status = status_of(&cid);
            let paid = earned(e);
            if status == "removed" && paid <= Decimal::ZERO {
                return None;
            }
            let pool: Decimal = e.rewards_config.iter().map(|r| r.rate_per_day).sum();
            Some(RewardRow {
                label: e.question.clone(),
                slug: e.market_slug.clone(),
                status,
                share: format!("{:.2}%", e.earning_percentage),
                earned_today: format!("${paid:.4}"),
                pool_per_day: if pool > Decimal::ZERO { format!("${:.0}/day", pool) } else { "—".into() },
                earned: paid,
                share_pct: e.earning_percentage,
            })
        })
        .collect();

    // Farmed markets Polymarket didn't list (no share and nothing paid yet).
    let mut seen: Vec<String> = entries.iter().map(|e| e.condition_id.to_string().to_lowercase()).collect();
    for c in configs {
        let cid = c.condition_id.to_lowercase();
        if seen.contains(&cid) {
            continue;
        }
        seen.push(cid);
        rows.push(RewardRow {
            label: c.label.clone(),
            slug: slug_of(&c.url),
            status: status_of(&c.condition_id),
            share: "0.00%".into(),
            earned_today: "$0.0000".into(),
            pool_per_day: "—".into(),
            earned: Decimal::ZERO,
            share_pct: Decimal::ZERO,
        });
    }
    rows.sort_by(|a, b| b.earned.cmp(&a.earned).then(b.share_pct.cmp(&a.share_pct)));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payout(day: NaiveDate, amount: Decimal) -> DailyPayout {
        let paid_at = (day + chrono::Duration::days(1)).and_hms_opt(0, 0, 10).unwrap().and_utc();
        DailyPayout { earned_day: day, paid_at, amount }
    }

    #[test]
    fn series_fills_missing_days_and_sums() {
        let yesterday = Utc::now().date_naive() - chrono::Duration::days(1);
        let days = vec![payout(yesterday, dec!(2)), payout(yesterday - chrono::Duration::days(10), dec!(5))];
        let s = history_series(&days, 30);
        assert_eq!(s.len(), 30);
        assert_eq!(s.last().unwrap(), &(yesterday, dec!(2)));
        assert_eq!(sum_last_days(&s, 7), dec!(2));
        assert_eq!(sum_last_days(&s, 30), dec!(7));
        assert!(rewards_chart_svg(&s).contains("<rect"));
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(1_234), "1,234");
        assert_eq!(thousands(171_709), "171,709");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    fn config(id: &str, cid: u8, paused: bool) -> MarketConfig {
        serde_json::from_value(serde_json::json!({
            "id": id, "url": "https://polymarket.com/event/ev/farmed-market", "label": format!("Farmed {id}"),
            "condition_id": format!("0x{cid:064x}"), "token_id": "1", "token_label": "Yes",
            "tick_size": "0.01", "distance": "0.01", "min_depth_between": "0", "order_size": "50",
            "expires_at": "2099-01-01T00:00:00Z", "paused": paused,
        }))
        .unwrap()
    }

    #[test]
    fn table_keeps_farmed_and_paid_markets_only() {
        // 1: farmed + live, 2: farmed but paused, 3: removed but paid today,
        // 4: someone else's market with nothing for us, 5: farmed, not listed yet.
        let entries = vec![entry(1, &["0.10"]), entry(2, &["0.40"]), entry(3, &["0.05"]), entry(4, &["0"])];
        let configs = vec![config("a", 1, false), config("b", 2, true), config("c", 5, false)];
        let mut status = HashMap::new();
        status.insert("a".to_string(), OrderStatus::Live { order_id: "o".into(), price: dec!(0.5) });

        let rows = build_table(&entries, &configs, &status);
        let got: Vec<(&str, &str)> = rows.iter().map(|r| (r.status, r.earned_today.as_str())).collect();
        assert_eq!(
            got,
            vec![("paused", "$0.4000"), ("farming", "$0.1000"), ("removed", "$0.0500"), ("waiting", "$0.0000")]
        );
        assert_eq!(rows[3].slug, "farmed-market");
    }

    fn entry(cid: u8, earnings: &[&str]) -> UserRewardsEarningResponse {
        let zero = format!("0x{}", "0".repeat(40));
        let earnings: Vec<_> = earnings
            .iter()
            .map(|e| serde_json::json!({ "asset_address": zero, "earnings": e, "asset_rate": "1" }))
            .collect();
        serde_json::from_value(serde_json::json!({
            "condition_id": format!("0x{cid:064x}"),
            "question": "Q", "market_slug": "m", "event_slug": "e", "image": "",
            "rewards_max_spread": "3", "rewards_min_size": "50", "market_competitiveness": "1",
            "maker_address": zero, "earning_percentage": "1.5", "earnings": earnings,
        }))
        .unwrap()
    }

    #[test]
    fn today_total_is_the_sum_of_the_market_rows() {
        let entries = vec![entry(1, &["0.25", "0.05"]), entry(2, &["1.10"]), entry(3, &[])];
        assert_eq!(earned(&entries[0]), dec!(0.30));
        assert_eq!(total_earned(&entries), dec!(1.40));
        assert_eq!(total_earned(&[]), Decimal::ZERO);
    }
}
