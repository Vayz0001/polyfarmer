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
use crate::types::{MarketConfig, OrderStatus};

use super::activity::{recent_views, AlertView};
use super::auth::csrf_token;
use super::rewards::{
    fetch_today_entries, history_series, load_payouts, rewards_chart_svg, sum_last_days, today_key, total_earned,
    yesterday_amount, yesterday_payout_swr,
};
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
    /// Scoring weight of the live order after Polymarket's two-sided rule, "19%".
    pub weight: Option<String>,
    /// Hover text explaining `weight` (two-sided, one-sided ÷3, or nothing).
    pub weight_note: String,
    /// Polymarket's own verdict on whether the live order is scoring.
    pub scoring: Option<bool>,
    /// How the engine places this leg: "2.0¢ below best bid".
    pub peg: String,
    pub size: String,
    /// Shares resting for this leg: exact for a live order ("556"), an
    /// estimate otherwise ("~556", from the current best bid and distance).
    pub shares: Option<String>,
    pub min_depth: String,
    pub expires: String,
    pub expires_title: String,
    pub auto_pause: Option<String>,
    token_id: String,
    mid_value: Option<Decimal>,
    /// This order's own weight, before the two-sided rule (live legs only).
    raw_weight: Option<Decimal>,
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
async fn scoring_swr(state: &WebState, mut live_ids: Vec<String>) -> HashMap<String, bool> {
    if live_ids.is_empty() {
        return HashMap::new();
    }
    let Some((executor, _)) = state.engine_handle.get().await else { return HashMap::new() };
    // Keyed by the exact set of live orders, so a removed, re-pegged or new
    // order is looked up at once instead of waiting out a stale entry.
    live_ids.sort_unstable();
    let key = live_ids.join(",");
    state
        .caches
        .scoring
        .get_swr(&key, move || async move {
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
        let tops: HashMap<String, (Option<Decimal>, Option<Decimal>)> =
            s.books.iter().map(|(t, b)| (t.clone(), (b.best_bid, b.best_ask))).collect();
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
            let mut legs: Vec<LegRow> = legs_cfg
                .iter()
                .enumerate()
                .map(|(i, c)| leg_row(c, i, order_status.get(&c.id), tops.get(&c.token_id), max_spread, &scoring))
                .collect();
            apply_two_sided(&mut legs);
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

/// Polymarket scores a market's two sides together: with both sides live the
/// weaker one sets the score; one side alone scores a third (midpoint 10–90¢)
/// or nothing (outside). Each leg's raw weight becomes that market score.
fn apply_two_sided(legs: &mut [LegRow]) {
    let live: Vec<(String, Decimal)> = legs.iter().filter_map(|l| Some((l.token_id.clone(), l.raw_weight?))).collect();
    for leg in legs.iter_mut() {
        let (Some(w), Some(mid)) = (leg.raw_weight, leg.mid_value) else { continue };
        let other = live.iter().filter(|(t, _)| *t != leg.token_id).map(|(_, w)| *w).max();
        let effective = market_data::effective_weight(mid, w, other);
        leg.weight = Some(pct(effective));
        leg.weight_note = match other {
            Some(o) if o > dec!(0) => {
                format!("Two-sided: the weaker side sets the score. This order alone: {}.", pct(w))
            }
            _ if market_data::single_sided_allowed(mid) => format!(
                "One-sided: Polymarket divides the score by 3. This order alone: {}. Farm both sides for full weight.",
                pct(w)
            ),
            _ => "One-sided orders earn nothing while the midpoint is under 10¢ or over 90¢. Farm both sides."
                .to_string(),
        };
    }
}

/// Shares behind a leg's USD size, as the engine would compute them
/// (`Executor::shares_from_usd`: size / price, 2dp). Uses the live order's
/// price when there is one; otherwise estimates the next bid from the book
/// (best bid − distance, snapped down to a tick) and prefixes "~".
fn shares_label(c: &MarketConfig, live_price: Option<Decimal>, best_bid: Option<Decimal>) -> Option<String> {
    let (price, estimated) = match (live_price, best_bid) {
        (Some(p), _) => (p, false),
        (None, Some(bb)) => (crate::engine::orderbook::TokenBook::snap_to_tick(bb - c.distance, c.tick_size), true),
        _ => return None,
    };
    if price <= dec!(0) {
        return None;
    }
    let shares = (c.order_size / price).round_dp(0);
    Some(format!("{}{shares} shares", if estimated { "~" } else { "" }))
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
    let (price, dmid, in_zone, raw_weight, is_scoring) = match (&live, mid) {
        (Some((oid, p)), Some(m)) => {
            let w = max_spread.map(|v| market_data::score_weight(m, *p, v));
            (
                Some(cents(*p)),
                Some(format!("{:.1}", (m - *p) * dec!(100))),
                w.map(|w| w > dec!(0)),
                w,
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
        // Set by `apply_two_sided` once the market's other leg is known.
        weight: raw_weight.map(pct),
        weight_note: String::new(),
        scoring: is_scoring,
        peg: format!("{}¢ below bid", cents(c.distance)),
        size: format!("${}", c.order_size.normalize()),
        shares: shares_label(c, live.as_ref().map(|(_, p)| *p), top.and_then(|(b, _)| *b)),
        min_depth: if c.min_depth_between > dec!(0) { usd(c.min_depth_between) } else { "—".to_string() },
        expires: until(c.expires_at),
        expires_title: c.expires_at.format("%b %-d, %Y %H:%M UTC").to_string(),
        auto_pause: c.max_volatility.map(|v| format!("{}¢", cents(v))),
        token_id: c.token_id.clone(),
        mid_value: mid,
        raw_weight,
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
    alerts: Vec<AlertView>,
}

pub async fn overview(State(state): State<WebState>, session: Session) -> Html<String> {
    let st = engine_status(&state).await;
    let rows = build_rows(&state).await;
    render(&OverviewTemplate {
        shell: shell(&session, "overview").await,
        st,
        has_wallet: state.store.has_wallet(),
        rows,
        alerts: recent_views(&state, 6),
    })
}

#[derive(Template)]
#[template(path = "_overview_rewards.html")]
struct OverviewRewardsTemplate {
    error: Option<String>,
    chart_svg: String,
    rewards_yesterday: String,
    rewards_7d: String,
    rewards_30d: String,
}

/// GET /overview/rewards — last 7 / 30 days of payouts and the daily chart.
pub async fn overview_rewards(State(state): State<WebState>) -> Html<String> {
    match load_payouts(&state).await {
        Ok(days) => {
            let series = history_series(&days, 30);
            render(&OverviewRewardsTemplate {
                error: None,
                chart_svg: rewards_chart_svg(&series),
                rewards_yesterday: usd(yesterday_amount(&days)),
                rewards_7d: usd(sum_last_days(&series, 7)),
                rewards_30d: usd(sum_last_days(&series, 30)),
            })
        }
        Err(e) => render(&OverviewRewardsTemplate {
            error: Some(e),
            chart_svg: String::new(),
            rewards_yesterday: String::new(),
            rewards_7d: String::new(),
            rewards_30d: String::new(),
        }),
    }
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
    rewards_yesterday: Option<String>,
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
    // Today's rewards come from a slow call: show the cached total and refresh
    // it in the background so the other numbers never wait for it.
    let cache = &state.caches.reward_markets;
    let today = match executor.clone() {
        Some(executor) => cache.get_swr(&today_key(), move || fetch_today_entries(executor)),
        None => cache.peek(&today_key()).map(|(v, _)| v),
    }
    .map(|entries| total_earned(&entries));
    let value = value_fut.await;
    let st = engine_status(&state).await;

    render(&KpisTemplate {
        running: executor.is_some(),
        balance: balance.map(usd),
        in_orders: usd(in_orders),
        live_legs,
        portfolio_value: value.map(usd),
        rewards_today: today.map(|t| format!("${t:.2}")),
        rewards_yesterday: yesterday_payout_swr(&state).map(|y| format!("${y:.2}")),
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
    fn shares_follow_the_live_price_or_estimate_from_the_book() {
        let c = MarketConfig {
            id: "x".into(),
            url: String::new(),
            label: String::new(),
            condition_id: String::new(),
            token_id: String::new(),
            token_label: String::new(),
            tick_size: dec!(0.01),
            distance: dec!(0.02),
            min_depth_between: dec!(0),
            order_size: dec!(100),
            expires_at: Utc::now(),
            paused: false,
            benchmark_bid: None,
            max_volatility: None,
        };
        // Live at 18c → 100 / 0.18 = 556 shares, exact.
        assert_eq!(shares_label(&c, Some(dec!(0.18)), Some(dec!(0.20))).as_deref(), Some("556 shares"));
        // Not live: best bid 20c − 2c = 18c → same, marked as an estimate.
        assert_eq!(shares_label(&c, None, Some(dec!(0.20))).as_deref(), Some("~556 shares"));
        // Nothing to go on.
        assert_eq!(shares_label(&c, None, None), None);
        // Degenerate price.
        assert_eq!(shares_label(&c, None, Some(dec!(0.02))), None);
    }

    fn leg(id: &str, token: &str, slot: usize, live_price: Option<Decimal>, mid: Decimal) -> LegRow {
        let c = MarketConfig {
            id: id.into(),
            url: String::new(),
            label: String::new(),
            condition_id: "0xabc".into(),
            token_id: token.into(),
            token_label: String::new(),
            tick_size: dec!(0.01),
            distance: dec!(0.01),
            min_depth_between: dec!(0),
            order_size: dec!(100),
            expires_at: Utc::now() + chrono::Duration::days(1),
            paused: live_price.is_none(),
            benchmark_bid: None,
            max_volatility: None,
        };
        let status = live_price.map(|price| OrderStatus::Live { order_id: format!("o{id}"), price });
        // A 2¢-wide book around `mid`.
        let top = (Some(mid - dec!(0.01)), Some(mid + dec!(0.01)));
        leg_row(&c, slot, status.as_ref(), Some(&top), Some(dec!(4)), &HashMap::new())
    }

    #[test]
    fn score_follows_the_two_sided_rule() {
        // Yes at mid 50¢, bid 49¢ (1¢ off, zone 4¢): alone ((4−1)/4)² = 56%.
        // No at mid 50¢, bid 48¢ (2¢ off): alone 25%.
        let both =
            || vec![leg("y", "yes", 0, Some(dec!(0.49)), dec!(0.50)), leg("n", "no", 1, Some(dec!(0.48)), dec!(0.50))];

        // Both sides live: the weaker side (25%) sets the score for both.
        let mut legs = both();
        apply_two_sided(&mut legs);
        assert_eq!(legs[0].weight.as_deref(), Some("25%"));
        assert_eq!(legs[1].weight.as_deref(), Some("25%"));
        assert!(legs[0].weight_note.starts_with("Two-sided"));

        // The No leg paused (or deleted): Yes alone scores 56% ÷ 3 = 19%.
        let mut legs = vec![leg("y", "yes", 0, Some(dec!(0.49)), dec!(0.50)), leg("n", "no", 1, None, dec!(0.50))];
        apply_two_sided(&mut legs);
        assert_eq!(legs[0].weight.as_deref(), Some("19%"));
        assert!(legs[0].weight_note.contains("divides the score by 3") && legs[0].weight_note.contains("56%"));
        assert_eq!(legs[1].weight, None, "a paused leg has no score");
        let mut alone = vec![leg("y", "yes", 0, Some(dec!(0.49)), dec!(0.50))];
        apply_two_sided(&mut alone);
        assert_eq!(alone[0].weight.as_deref(), Some("19%"));

        // One-sided with the midpoint under 10¢: nothing.
        let mut low = vec![leg("y", "yes", 0, Some(dec!(0.05)), dec!(0.06))];
        apply_two_sided(&mut low);
        assert_eq!(low[0].weight.as_deref(), Some("0%"));
        assert!(low[0].weight_note.contains("earn nothing"));
    }

    #[test]
    fn slug_is_last_segment() {
        assert_eq!(slug_of("https://polymarket.com/event/ev/mk"), "mk");
        assert_eq!(slug_of("https://polymarket.com/event/mk/"), "mk");
    }
}
