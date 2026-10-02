//! Overview + Markets pages — views over the engine's live [`AppState`].
//! View-models are built here (not in templates) so Askama stays simple:
//! plain strings, no Decimal/enum formatting in markup.
//!
//! Nothing here blocks on Polymarket: per-market reward params come from the
//! Gamma cache (stale-while-revalidate), reward scoring from a 15s cache, and
//! the slow account reads (balance, portfolio value, rewards today, fills)
//! load as lazy fragments.

use std::collections::HashMap;
use std::time::Duration;

use askama::Template;
use axum::extract::State;
use axum::response::Html;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tower_sessions::Session;

use crate::rewards::{gamma_resolve, market_data, portfolio};
use crate::storage::load_reward_history;
use crate::types::{MarketConfig, OrderStatus};

use super::activity::{recent_views, AlertView};
use super::auth::csrf_token;
use super::rewards::{history_series, rewards_chart_svg, sum_last_days};
use super::shell::{balance_swr, engine_status, render, shell, EngineStatus, Shell};
use super::state::WebState;

const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(8);

// ── Formatting helpers (shared with other web modules) ─────────────────────

/// Price units → cents with one decimal: 0.455 → "45.5".
pub(super) fn cents(p: Decimal) -> String {
    format!("{:.1}", p * dec!(100))
}

/// Weight in [0, 1] → rounded whole percent ("53%"). Decimal's `{:.0}`
/// truncates, so round explicitly.
pub(super) fn pct(w: Decimal) -> String {
    format!("{}%", (w * dec!(100)).round_dp(0))
}

/// "$1,234.56"-ish without thousands separators (tabular, compact).
pub(super) fn usd(v: Decimal) -> String {
    if v.abs() >= dec!(1000) {
        market_data::fmt_usd(v)
    } else {
        format!("${v:.2}")
    }
}

/// Time until `t`: "6d 4h", "3h 12m", "45m", "expired", or "never" for the
/// far-future sentinel used by "Never" expiry.
pub(super) fn until(t: DateTime<Utc>) -> String {
    let secs = (t - Utc::now()).num_seconds();
    if secs <= 0 {
        return "expired".to_string();
    }
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 3650 {
        "never".to_string()
    } else if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{}m", m.max(1))
    }
}

/// Market slug = last path segment of the stored event URL.
pub(super) fn slug_of(url: &str) -> String {
    url.trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string()
}

// ── Leg / market rows ────────────────────────────────────────────────────────

/// One outcome leg — one side of a one- or both-sides farming config.
pub struct LegRow {
    /// Config id for this leg — pause/resume/edit/remove act on this leg.
    pub id: String,
    pub paused: bool,
    pub side_label: String,
    /// "a" | "b" — outcome slot (first/second token), for a stable,
    /// outcome-agnostic colour split. Never matches on "yes"/"no" text.
    pub side_class: &'static str,
    pub status_label: &'static str,
    /// "live" | "busy" | "idle" | "paused" | "expired"
    pub status_class: &'static str,
    /// Resting price (¢) when live.
    pub price: Option<String>,
    /// Current midpoint (¢) from the engine's live book.
    pub mid: Option<String>,
    /// Distance of the live order below the midpoint (¢).
    pub dmid: Option<String>,
    /// Reward-zone fit of the live order: Some(true) in zone, Some(false)
    /// drifted out, None unknown (not live / no reward params yet).
    pub in_zone: Option<bool>,
    /// Scoring weight of the live order, "56%".
    pub weight: Option<String>,
    /// Polymarket's own verdict on whether the live order is scoring.
    pub scoring: Option<bool>,
    /// How the engine places this leg: "2.0¢ below best bid".
    pub peg: String,
    pub size: String,
    pub min_depth: String,
    pub expires: String,
    pub expires_title: String,
    pub auto_pause: Option<String>,
}

/// One market (grouped by `condition_id`) with its 1–2 legs.
pub struct MarketRow {
    pub condition_id: String,
    pub label: String,
    /// Polymarket URL (external link).
    pub url: String,
    /// Slug for the internal market view (`/markets/view?slug=`).
    pub slug: String,
    pub max_spread: Option<String>,
    pub legs: Vec<LegRow>,
    pub all_paused: bool,
}

/// Scoring verdicts for every live order, from a 15s cache — never blocks.
async fn scoring_swr(state: &WebState, live_ids: Vec<String>) -> HashMap<String, bool> {
    if live_ids.is_empty() {
        return HashMap::new();
    }
    let Some((executor, _)) = state.engine_handle.get().await else { return HashMap::new() };
    state
        .caches
        .scoring
        .get_swr("live", move || async move {
            let refs: Vec<&str> = live_ids.iter().map(String::as_str).collect();
            tokio::time::timeout(ACCOUNT_TIMEOUT, executor.are_orders_scoring(&refs))
                .await
                .map_err(|_| eyre::eyre!("timeout"))
                .and_then(|r| r)
        })
        .unwrap_or_default()
}

pub(super) async fn build_rows(state: &WebState) -> Vec<MarketRow> {
    let (configs, order_status, tops) = {
        let s = state.engine.read().await;
        // Top of book per token, from the engine's live WS-fed books.
        let tops: HashMap<String, (Option<Decimal>, Option<Decimal>)> = s
            .books
            .iter()
            .map(|(t, b)| (t.clone(), (b.best_bid, b.best_ask)))
            .collect();
        (s.configs.clone(), s.order_status.clone(), tops)
    };

    let live_ids: Vec<String> = order_status
        .values()
        .filter_map(|st| match st {
            OrderStatus::Live { order_id, .. } => Some(order_id.clone()),
            _ => None,
        })
        .collect();
    let scoring = scoring_swr(state, live_ids).await;

    // Group configs by condition_id, preserving first-seen order, so a
    // both-sides farm (2 configs, 1 condition_id) renders as one market.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&MarketConfig>> = HashMap::new();
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
            let first = legs_cfg[0];
            let slug = slug_of(&first.url);
            let max_spread = gamma_resolve::market_by_slug_swr(&slug).and_then(|m| m.rewards_max_spread);
            let legs: Vec<LegRow> = legs_cfg
                .iter()
                .enumerate()
                .map(|(i, c)| leg_row(c, i, order_status.get(&c.id), tops.get(&c.token_id), max_spread, &scoring))
                .collect();
            MarketRow {
                condition_id: cid,
                label: first.label.clone(),
                url: first.url.clone(),
                slug,
                max_spread: max_spread.map(|v| format!("{}¢", v.normalize())),
                all_paused: legs.iter().all(|l| l.paused),
                legs,
            }
        })
        .collect()
}

fn leg_row(
    c: &MarketConfig,
    slot: usize,
    status: Option<&OrderStatus>,
    top: Option<&(Option<Decimal>, Option<Decimal>)>,
    max_spread: Option<Decimal>,
    scoring: &HashMap<String, bool>,
) -> LegRow {
    let mid = match top {
        Some((Some(b), Some(a))) => Some((*b + *a) / dec!(2)),
        _ => None,
    };
    let expired = c.expires_at <= Utc::now();
    let (status_label, status_class, live) = if c.paused {
        ("Paused", "paused", None)
    } else {
        match status {
            Some(OrderStatus::Live { order_id, price }) => ("Live", "live", Some((order_id.clone(), *price))),
            Some(OrderStatus::Placing { .. }) => ("Placing", "busy", None),
            Some(OrderStatus::Cancelling { .. }) => ("Cancelling", "busy", None),
            _ if expired => ("Expired", "expired", None),
            _ => ("Waiting", "idle", None),
        }
    };
    let (price, dmid, in_zone, weight, is_scoring) = match (&live, mid) {
        (Some((oid, p)), Some(m)) => {
            let w = max_spread.map(|v| market_data::score_weight(m, *p, v));
            (
                Some(cents(*p)),
                Some(format!("{:.1}", (m - *p) * dec!(100))),
                w.map(|w| w > dec!(0)),
                w.map(pct),
                scoring.get(oid).copied(),
            )
        }
        (Some((oid, p)), None) => (Some(cents(*p)), None, None, None, scoring.get(oid).copied()),
        _ => (None, None, None, None, None),
    };
    LegRow {
        id: c.id.clone(),
        paused: c.paused,
        side_label: c.token_label.clone(),
        side_class: if slot == 0 { "a" } else { "b" },
        status_label,
        status_class,
        price,
        mid: mid.map(cents),
        dmid,
        in_zone,
        weight,
        scoring: is_scoring,
        peg: format!("{}¢ below bid", cents(c.distance)),
        size: format!("${}", c.order_size.normalize()),
        min_depth: if c.min_depth_between > dec!(0) { usd(c.min_depth_between) } else { "—".to_string() },
        expires: until(c.expires_at),
        expires_title: c.expires_at.format("%b %-d, %Y %H:%M UTC").to_string(),
        auto_pause: c.max_volatility.map(|v| format!("{}¢", cents(v))),
    }
}

// ── Overview ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "index.html")]
struct OverviewTemplate {
    shell: Shell,
    st: EngineStatus,
    has_wallet: bool,
    rows: Vec<MarketRow>,
    chart_svg: String,
    rewards_7d: String,
    rewards_30d: String,
    alerts: Vec<AlertView>,
}

pub async fn overview(State(state): State<WebState>, session: Session) -> Html<String> {
    let st = engine_status(&state).await;
    let rows = build_rows(&state).await;
    let history = load_reward_history(&state.reward_history_file).unwrap_or_default();
    let series = history_series(&history, 30);
    render(&OverviewTemplate {
        shell: shell(&session, "overview").await,
        st,
        has_wallet: state.store.has_wallet(),
        rows,
        chart_svg: rewards_chart_svg(&series),
        rewards_7d: usd(sum_last_days(&series, 7)),
        rewards_30d: usd(sum_last_days(&series, 30)),
        alerts: recent_views(&state, 6),
    })
}

#[derive(Template)]
#[template(path = "_overview_book.html")]
struct OverviewBookTemplate {
    rows: Vec<MarketRow>,
}

/// GET /overview/book — "Your book" panel, refreshed on state events.
pub async fn overview_book(State(state): State<WebState>) -> Html<String> {
    render(&OverviewBookTemplate { rows: build_rows(&state).await })
}

#[derive(Template)]
#[template(path = "_overview_kpis.html")]
struct KpisTemplate {
    running: bool,
    balance: Option<String>,
    in_orders: String,
    live_legs: usize,
    portfolio_value: Option<String>,
    rewards_today: Option<String>,
    markets: usize,
    paused_legs: usize,
}

/// GET /overview/kpis — account numbers (lazy: balance / value / today's
/// rewards are network reads, each cached).
pub async fn overview_kpis(State(state): State<WebState>) -> Html<String> {
    let (in_orders, live_legs) = {
        let s = state.engine.read().await;
        s.configs.iter().fold((dec!(0), 0), |(sum, n), c| match s.order_status.get(&c.id) {
            Some(OrderStatus::Live { .. }) => (sum + c.order_size, n + 1),
            _ => (sum, n),
        })
    };
    let executor = state.engine_handle.get().await.map(|(e, _)| e);
    let balance = balance_swr(&state).await;

    let value_fut = async {
        let user = state.wallet_address()?;
        state
            .caches
            .value
            .get_or_fetch(&user, || async {
                tokio::time::timeout(ACCOUNT_TIMEOUT, portfolio::value(&user))
                    .await
                    .map_err(|_| eyre::eyre!("timeout"))?
            })
            .await
            .ok()
    };
    let today_fut = async {
        let executor = executor.clone()?;
        state
            .caches
            .rewards_today
            .get_or_fetch("today", || async {
                let today = Utc::now().date_naive();
                let entries = tokio::time::timeout(ACCOUNT_TIMEOUT, executor.total_earnings_for_user_for_day(today))
                    .await
                    .map_err(|_| eyre::eyre!("timeout"))??;
                Ok::<_, eyre::Report>(entries.iter().map(|e| e.earnings).sum::<Decimal>())
            })
            .await
            .ok()
    };
    let (value, today) = tokio::join!(value_fut, today_fut);
    let st = engine_status(&state).await;

    render(&KpisTemplate {
        running: executor.is_some(),
        balance: balance.map(usd),
        in_orders: usd(in_orders),
        live_legs,
        portfolio_value: value.map(usd),
        rewards_today: today.map(|t| format!("${t:.2}")),
        markets: st.markets,
        paused_legs: st.paused_legs,
    })
}

pub struct FillRow {
    pub ts_iso: String,
    pub time: String,
    pub title: String,
    pub slug: String,
    pub side: String,
    pub outcome: String,
    pub size: String,
    pub price: String,
    pub usdc: String,
    /// On a market this bot is farming — i.e. one of OUR resting bids got hit.
    pub tracked: bool,
}

pub(super) fn fill_rows(acts: &[portfolio::Activity], tracked: &[String], limit: usize) -> Vec<FillRow> {
    acts.iter()
        .filter(|a| a.kind == "TRADE")
        .take(limit)
        .map(|a| {
            let ts = DateTime::<Utc>::from_timestamp(a.timestamp, 0).unwrap_or_else(Utc::now);
            FillRow {
                ts_iso: ts.to_rfc3339(),
                time: ts.format("%b %-d %H:%M").to_string(),
                title: a.title.clone(),
                slug: a.slug.clone(),
                side: a.side.clone(),
                outcome: a.outcome.clone(),
                size: format!("{:.1}", a.size),
                price: cents(a.price),
                usdc: format!("${:.2}", a.usdc_size),
                tracked: tracked.iter().any(|c| c.eq_ignore_ascii_case(&a.condition_id)),
            }
        })
        .collect()
}

/// Recent wallet activity via the Data API, cached.
pub(super) async fn recent_activity(state: &WebState) -> Result<Vec<portfolio::Activity>, String> {
    let user = state.wallet_address().ok_or_else(|| "No wallet configured yet.".to_string())?;
    state
        .caches
        .activity
        .get_or_fetch(&user, || async {
            tokio::time::timeout(ACCOUNT_TIMEOUT, portfolio::activity(&user, 100))
                .await
                .map_err(|_| eyre::eyre!("timeout"))?
        })
        .await
        .map_err(|e| format!("Could not load activity: {e}"))
}

#[derive(Template)]
#[template(path = "_fills_table.html")]
struct FillsTemplate {
    rows: Vec<FillRow>,
    error: Option<String>,
    compact: bool,
}

/// GET /overview/fills — recent fills (Data API TRADE activity).
pub async fn overview_fills(State(state): State<WebState>) -> Html<String> {
    let tracked: Vec<String> = state.engine.read().await.configs.iter().map(|c| c.condition_id.clone()).collect();
    match recent_activity(&state).await {
        Ok(acts) => render(&FillsTemplate { rows: fill_rows(&acts, &tracked, 8), error: None, compact: true }),
        Err(e) => render(&FillsTemplate { rows: Vec::new(), error: Some(e), compact: true }),
    }
}

// ── Markets ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "markets.html")]
struct MarketsTemplate {
    shell: Shell,
    has_wallet: bool,
    csrf_token: String,
    rows: Vec<MarketRow>,
    flash: Option<String>,
}

pub async fn markets(State(state): State<WebState>, session: Session) -> Html<String> {
    let rows = build_rows(&state).await;
    render(&MarketsTemplate {
        shell: shell(&session, "markets").await,
        has_wallet: state.store.has_wallet(),
        csrf_token: csrf_token(&session).await,
        rows,
        flash: None,
    })
}

#[derive(Template)]
#[template(path = "_markets_table.html")]
struct MarketsTableTemplate {
    csrf_token: String,
    rows: Vec<MarketRow>,
    flash: Option<String>,
}

/// The markets table fragment — target of state-event refreshes and of every
/// row action (which return it re-rendered, with an error banner on failure).
pub(super) async fn render_markets_table(state: &WebState, session: &Session, flash: Option<String>) -> Html<String> {
    let rows = build_rows(state).await;
    render(&MarketsTableTemplate { csrf_token: csrf_token(session).await, rows, flash })
}

pub async fn markets_table(State(state): State<WebState>, session: Session) -> Html<String> {
    render_markets_table(&state, &session, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn until_formats_ranges() {
        assert_eq!(until(Utc::now() - chrono::Duration::minutes(1)), "expired");
        assert_eq!(until(Utc::now() + chrono::Duration::days(36500)), "never");
        assert!(until(Utc::now() + chrono::Duration::hours(50)).starts_with("2d"));
        assert!(until(Utc::now() + chrono::Duration::minutes(90)).starts_with("1h"));
    }

    #[test]
    fn slug_is_last_segment() {
        assert_eq!(slug_of("https://polymarket.com/event/ev/mk"), "mk");
        assert_eq!(slug_of("https://polymarket.com/event/mk/"), "mk");
    }
}
