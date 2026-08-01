//! Market lifecycle: browse reward-eligible markets, resolve a pasted URL,
//! and add/remove/pause/resume. Write-through — handlers mutate `AppState`
//! directly and persist atomically, replacing the old 3s markets.json poll
//! loop (which had nothing left to reconcile once this became the only
//! write path).

use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use chrono::Utc;
use polymarket_client_sdk_v2::clob::types::Interval;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use tower_sessions::Session;

use crate::rewards::{gamma_resolve, market_data, markets_browse};
use crate::storage::save_markets;
use crate::types::{MarketConfig, OrderStatus, WsCommand};

use super::auth::{csrf_token, verify_csrf};
use super::state::WebState;

/// Hard upper bound on any handler-initiated network call (Gamma resolve,
/// the browse endpoint) — background tasks can block indefinitely without
/// harming anything, but a request handler must not hang on a flaky upstream.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(8);
/// `user_earnings_and_markets_config` scans every reward-eligible market server-
/// side (can't be scoped to one market) — give it a longer budget, like the
/// Rewards page does. Only runs in the async Your-position fragment.
const POSITION_EARNINGS_TIMEOUT: Duration = Duration::from_secs(20);
/// Polymarket's pagination sentinel (base64 of "-1") — shared by every
/// paginated rewards endpoint, confirmed via the official API reference.
const TERMINAL_CURSOR: &str = "LTE=";
/// How many book levels per side the ladder shows — deep enough that scrolling
/// reveals real depth (not just a cent or two). The ladder area scrolls
/// (max-height in CSS), so a tall book never grows the page.
const LADDER_LEVELS: usize = 50;
/// Sensible starting order size ($) prefilled in the placement panel.
const DEFAULT_ORDER_SIZE: &str = "100";

// ── Interactive market view (chart + order book + visual placement) ──────────

#[derive(Deserialize)]
pub struct ViewParams {
    pub slug: String,
    /// Which outcome (0 or 1) is selected. Defaults to 0.
    #[serde(default)]
    pub side: usize,
    /// "one" | "both" — farm one outcome or both legs. Defaults to "one".
    pub sides: Option<String>,
}

/// One outcome's pre-rendered view data. Both sides are embedded in the page so
/// switching the outcome tab is an instant client-side swap (no network round-
/// trip) — only the live order book refreshes async for the selected side.
pub struct SideView {
    pub idx: usize,
    pub label: String,
    pub price_cents: String,
    pub midpoint_cents: String,
    // One pre-rendered chart per time range, so range + outcome toggles are both
    // instant client-side swaps (no network round-trip).
    pub chart_1d: String,
    pub chart_1w: String,
    pub chart_all: String,
    // Caption per range (change + range over that window) so it always matches
    // the chart on screen.
    pub cap_1d: String,
    pub cap_1w: String,
    pub cap_all: String,
    pub suggested_price: Option<String>,
    pub selected: bool,
}

/// An order-book price-grouping choice (raw tick + coarser cent buckets).
pub struct GroupOpt {
    pub value: String, // group size in price units, e.g. "0.001" (book URL param)
    pub label: String, // "0.1¢" / "1¢" / "5¢"
    pub selected: bool,
}

/// Grouping choices: the market's raw tick plus up to two coarser cent buckets.
/// Default (first) = raw tick, preserving click-to-place precision.
fn group_options(tick: Decimal) -> Vec<GroupOpt> {
    let tick_cents = tick * dec!(100);
    let mut cents: Vec<Decimal> = vec![tick_cents];
    for c in [dec!(1), dec!(5), dec!(10)] {
        if c > tick_cents {
            cents.push(c);
        }
    }
    cents.truncate(3);
    cents.into_iter().enumerate().map(|(i, c)| GroupOpt {
        value: (c / dec!(100)).normalize().to_string(),
        label: format!("{}\u{00a2}", c.normalize()),
        selected: i == 0,
    }).collect()
}

#[derive(Template)]
#[template(path = "market_view.html")]
struct MarketViewTemplate {
    csrf_token: String,
    slug: String,
    condition_id: String,
    question: String,
    image: Option<String>,
    group_item_title: Option<String>,
    has_rewards: bool,
    reward_min: String,
    reward_max_spread: String,
    volume_24hr: Option<String>,
    liquidity: Option<String>,
    resolves: Option<String>,
    countdown: Option<String>,
    sides: Vec<SideView>,
    selected_idx: usize,
    selected_label: String,
    selected_mid: String,
    selected_cap: String,
    group_opts: Vec<GroupOpt>,
    selected_suggested: Option<String>,
    sides_both: bool,
    ladder: market_data::Ladder,
    default_size: String,
    /// Market tick size (e.g. "0.01") — drives the price input's step/min so the
    /// up/down arrows move by one tick and stay tick-aligned.
    price_tick: String,
    /// Highest valid price = 1 - tick (e.g. "0.99").
    price_max: String,
    /// Reward minimum in SHARES (raw number, "" if no rewards) — feeds the
    /// instant client-side qualify calc.
    reward_min_shares: String,
    /// Reward max-spread in cents (raw number, "0" if none) — qualify calc.
    reward_max_spread_cents: String,
}

#[derive(Template)]
#[template(path = "market_view_error.html")]
struct MarketViewErrorTemplate {
    message: String,
}

/// GET /markets/view?slug=&side=&sides= — the trading-terminal-style view.
pub async fn market_view(session: Session, Query(p): Query<ViewParams>) -> Response {
    let mr = match tokio::time::timeout(NETWORK_TIMEOUT, gamma_resolve::market_by_slug(&p.slug)).await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return view_error(&format!("Could not load this market: {e}")),
        Err(_) => return view_error("Request to Polymarket timed out — try again."),
    };

    let side = p.side.min(mr.outcomes.len().saturating_sub(1));
    let token = mr.token_ids[side].clone();
    let max_spread_cents = mr.rewards_max_spread.unwrap_or(dec!(0));

    // Fetch every side's midpoint + price history across all three ranges (so
    // both the outcome tab and the 1D/1W/All range tabs are instant client-side
    // swaps) plus the selected side's order book — all concurrently, so page load
    // costs one round-trip's latency, not the sum.
    let hist = |iv: Interval| futures_util::future::join_all(mr.token_ids.iter().map(move |t| async move {
        tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_history(t, iv))
            .await.ok().and_then(Result::ok).unwrap_or_default()
    }));
    let mids_fut = futures_util::future::join_all(mr.token_ids.iter().map(|t| async move {
        tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_midpoint(t)).await.ok().and_then(Result::ok)
    }));
    let book_fut = tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(&token));
    let (mids, h1d, h1w, hall, book_res) = tokio::join!(
        mids_fut, hist(Interval::OneDay), hist(Interval::OneWeek), hist(Interval::Max), book_fut
    );
    let book = book_res.ok().and_then(Result::ok);

    let midpoint_of = |i: usize| mids.get(i).copied().flatten()
        .unwrap_or_else(|| mr.outcome_prices.get(i).copied().unwrap_or(dec!(0.5)));
    let suggest_of = |i: usize| mr.rewards_max_spread
        .and_then(|ms| market_data::suggest_placement(midpoint_of(i), ms, mr.tick_size));
    let chart = |hs: &[Vec<(i64, Decimal)>], i: usize|
        market_data::price_chart_svg(hs.get(i).map(Vec::as_slice).unwrap_or(&[]));
    let cap = |hs: &[Vec<(i64, Decimal)>], i: usize, label: &str|
        market_data::chart_caption(hs.get(i).map(Vec::as_slice).unwrap_or(&[]), label);

    let midpoint = midpoint_of(side);
    let group_opts = group_options(mr.tick_size);
    let ladder = match &book {
        // Default grouping = raw tick. "you are here" marker at the suggested price.
        Some(b) => market_data::build_ladder(b, midpoint, max_spread_cents, LADDER_LEVELS, suggest_of(side), mr.tick_size),
        None => empty_ladder(midpoint),
    };

    let sides: Vec<SideView> = mr.outcomes.iter().enumerate().map(|(i, label)| SideView {
        idx: i,
        label: label.clone(),
        price_cents: mr.outcome_prices.get(i).map(|p| fmt_cents(*p)).unwrap_or_else(|| "—".to_string()),
        midpoint_cents: fmt_cents(midpoint_of(i)),
        chart_1d: chart(&h1d, i),
        chart_1w: chart(&h1w, i),
        chart_all: chart(&hall, i),
        cap_1d: cap(&h1d, i, "24h"),
        cap_1w: cap(&h1w, i, "1W"),
        cap_all: cap(&hall, i, "All-time"),
        suggested_price: suggest_of(i).map(|p| p.normalize().to_string()),
        selected: i == side,
    }).collect();

    // Market-context facts (clean header meta — placed by relevance, not cards).
    let now = Utc::now();
    let resolves = mr.end_date.map(|d| d.format("%b %-d, %Y").to_string());
    let countdown = mr.end_date.map(|d| {
        let days = (d - now).num_days();
        if days > 1 { format!("{days}d left") }
        else if days == 1 { "1d left".to_string() }
        else if days == 0 { "ends today".to_string() }
        else { "ended".to_string() }
    });
    let liquidity = mr.liquidity.filter(|l| *l > dec!(0)).map(market_data::fmt_usd);

    let tpl = MarketViewTemplate {
        csrf_token: csrf_token(&session).await,
        slug: p.slug.clone(),
        condition_id: mr.condition_id.clone(),
        question: mr.question.clone(),
        image: mr.image.clone(),
        group_item_title: mr.group_item_title.clone().filter(|g| !g.is_empty()),
        has_rewards: mr.has_rewards(),
        reward_min: mr.rewards_min_size.map(|s| format!("{s} shares")).unwrap_or_else(|| "—".to_string()),
        reward_max_spread: mr.rewards_max_spread.map(|s| format!("{s}c")).unwrap_or_else(|| "—".to_string()),
        volume_24hr: mr.volume_24hr.filter(|v| *v > dec!(0)).map(market_data::fmt_usd),
        liquidity,
        resolves,
        countdown,
        selected_label: mr.outcomes.get(side).cloned().unwrap_or_default(),
        selected_mid: fmt_cents(midpoint),
        selected_cap: cap(&hall, side, "All-time"),
        group_opts,
        selected_suggested: suggest_of(side).map(|p| p.normalize().to_string()),
        sides_both: p.sides.as_deref() == Some("both"),
        sides,
        selected_idx: side,
        ladder,
        default_size: DEFAULT_ORDER_SIZE.to_string(),
        price_tick: mr.tick_size.normalize().to_string(),
        price_max: (dec!(1) - mr.tick_size).normalize().to_string(),
        reward_min_shares: mr.rewards_min_size.map(|s| s.normalize().to_string()).unwrap_or_default(),
        reward_max_spread_cents: mr.rewards_max_spread.map(|s| s.normalize().to_string()).unwrap_or_else(|| "0".to_string()),
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>"))).into_response()
}

fn view_error(message: &str) -> Response {
    let tpl = MarketViewErrorTemplate { message: message.to_string() };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>"))).into_response()
}

fn fmt_cents(price: Decimal) -> String {
    format!("{:.1}", price * dec!(100))
}

fn empty_ladder(mid: Decimal) -> market_data::Ladder {
    market_data::Ladder {
        asks: vec![],
        bids: vec![],
        midpoint_cents: fmt_cents(mid),
        spread_cents: "—".to_string(),
        best_bid_cents: "—".to_string(),
        best_ask_cents: "—".to_string(),
        band_lo_cents: "—".to_string(),
        band_hi_cents: "—".to_string(),
        in_zone_usdc: "—".to_string(),
        in_zone_raw: "0".to_string(),
        has_book: false,
    }
}

// ── Order book ladder (htmx-polled fragment) ────────────────────────────────

#[derive(Deserialize)]
pub struct BookParams {
    pub slug: String,
    #[serde(default)]
    pub side: usize,
    /// Your current placement price, so the live book can mark "you are here".
    pub price: Option<String>,
    /// Price-bucket size for the grouping control (defaults to the raw tick).
    pub group: Option<String>,
}

#[derive(Template)]
#[template(path = "_book_ladder.html")]
struct BookLadderTemplate {
    ladder: market_data::Ladder,
}

/// GET /markets/view/book?slug=&side= — htmx poll target for the live order
/// book (every 2s while the view is open; the market isn't engine-subscribed
/// yet at this point, so this reads the public REST book directly).
pub async fn view_book(Query(p): Query<BookParams>) -> Html<String> {
    let render = |ladder| {
        let tpl = BookLadderTemplate { ladder };
        Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
    };

    let mr = match tokio::time::timeout(NETWORK_TIMEOUT, gamma_resolve::market_by_slug(&p.slug)).await {
        Ok(Ok(m)) => m,
        _ => return render(empty_ladder(dec!(0.5))),
    };
    let side = p.side.min(mr.outcomes.len().saturating_sub(1));
    let token = &mr.token_ids[side];
    let max_spread_cents = mr.rewards_max_spread.unwrap_or(dec!(0));

    let your_price = p.price.as_deref().and_then(|s| s.trim().parse::<Decimal>().ok());
    let group = p.group.as_deref().and_then(|s| s.trim().parse::<Decimal>().ok())
        .filter(|g| *g > dec!(0)).unwrap_or(mr.tick_size);
    let midpoint = match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_midpoint(token)).await {
        Ok(Ok(m)) => m,
        _ => mr.outcome_prices.get(side).copied().unwrap_or(dec!(0.5)),
    };
    match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(token)).await {
        Ok(Ok(book)) => render(market_data::build_ladder(&book, midpoint, max_spread_cents, LADDER_LEVELS, your_price, group)),
        _ => render(empty_ladder(midpoint)),
    }
}

// ── Your position (authenticated; only when you're farming this market) ─────

#[derive(Deserialize)]
pub struct PositionParams {
    pub cid: String,
}

pub struct PositionLeg {
    pub label: String,
    pub status: String,
    pub scoring: Option<bool>,
}

#[derive(Template)]
#[template(path = "_your_position.html")]
struct YourPositionTemplate {
    show: bool,
    scoring_any: bool,
    reward_pct: Option<String>,
    earnings_today: Option<String>,
    legs: Vec<PositionLeg>,
}

impl YourPositionTemplate {
    fn hidden() -> Self {
        Self { show: false, scoring_any: false, reward_pct: None, earnings_today: None, legs: Vec::new() }
    }
}

/// GET /markets/view/position?cid= — htmx fragment, loaded async so the heavier
/// authenticated reward call never blocks the market page. Renders your live
/// status for this market only when the engine is running AND you're farming it;
/// otherwise renders nothing (the placeholder collapses).
pub async fn view_position(State(state): State<WebState>, Query(p): Query<PositionParams>) -> Html<String> {
    let render = |tpl: YourPositionTemplate| {
        Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
    };

    let (legs_cfg, order_status) = {
        let s = state.engine.read().await;
        let legs: Vec<MarketConfig> = s.configs.iter().filter(|c| c.condition_id == p.cid).cloned().collect();
        (legs, s.order_status.clone())
    };
    if legs_cfg.is_empty() {
        return render(YourPositionTemplate::hidden());
    }
    let Some((executor, _)) = state.engine_handle.get().await else {
        return render(YourPositionTemplate::hidden());
    };

    // Live order ids on this market → batch scoring check; plus the real
    // per-user %/$ (matched by condition_id). Run concurrently.
    let live_ids: Vec<String> = legs_cfg.iter().filter_map(|c| match order_status.get(&c.id) {
        Some(OrderStatus::Live { order_id, .. }) => Some(order_id.clone()),
        _ => None,
    }).collect();
    let scoring_fut = async {
        if live_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            let refs: Vec<&str> = live_ids.iter().map(String::as_str).collect();
            tokio::time::timeout(NETWORK_TIMEOUT, executor.are_orders_scoring(&refs))
                .await.ok().and_then(Result::ok).unwrap_or_default()
        }
    };
    let earn_fut = tokio::time::timeout(POSITION_EARNINGS_TIMEOUT, executor.user_earnings_and_markets_config(Utc::now().date_naive()));
    let (scoring, earn_res) = tokio::join!(scoring_fut, earn_fut);

    let (reward_pct, earnings_today) = match earn_res {
        Ok(Ok(entries)) => entries.iter()
            .find(|e| e.condition_id.to_string().eq_ignore_ascii_case(&p.cid))
            .map(|e| {
                let earned: Decimal = e.earnings.iter().map(|a| a.earnings).sum();
                (Some(format!("{:.1}%", e.earning_percentage)), Some(format!("${earned:.2}")))
            })
            .unwrap_or((None, None)),
        _ => (None, None),
    };

    let legs: Vec<PositionLeg> = legs_cfg.iter().map(|c| {
        let (status, scoring_leg) = match order_status.get(&c.id) {
            Some(OrderStatus::Live { order_id, price }) =>
                (format!("resting {}¢", fmt_cents(*price)), scoring.get(order_id).copied()),
            Some(OrderStatus::Placing { .. }) => ("placing…".to_string(), None),
            Some(OrderStatus::Cancelling { .. }) => ("cancelling…".to_string(), None),
            _ if c.paused => ("paused".to_string(), None),
            _ => ("idle".to_string(), None),
        };
        PositionLeg { label: c.token_label.clone(), status, scoring: scoring_leg }
    }).collect();

    let scoring_any = legs.iter().any(|l| l.scoring == Some(true));
    render(YourPositionTemplate { show: true, scoring_any, reward_pct, earnings_today, legs })
}

// ── Live placement preview (qualify / fill-risk feedback) ──────────────────

#[derive(Deserialize)]
pub struct PlacementPreviewForm {
    slug: String,
    #[serde(default)]
    side: usize,
    sides: Option<String>,
    price: String,
    order_size: String,
}

/// In both-sides mode, the DERIVED other leg: it rests at the same distance
/// below *its* best bid, so its price/qualify are shown for transparency (the
/// user only sets the selected side's price).
struct OtherLeg {
    label: String,
    price_cents: String,
    qualifies: bool,
    meets_min: bool,
    shares: String,
}

#[derive(Template)]
#[template(path = "_placement_preview.html")]
struct PlacementPreviewTemplate {
    has_rewards: bool,
    cents_from_mid: String,
    per_side_size: String,   // USD, "$X.XX"
    per_side_shares: String, // share count, "N"
    per_side_meets_min: bool,
    min_size: String,        // reward minimum, in shares
    fill_risk: &'static str,
    other_leg: Option<OtherLeg>,
    error: Option<String>,
}

impl PlacementPreviewTemplate {
    fn error(message: impl Into<String>) -> Self {
        Self {
            has_rewards: true,
            cents_from_mid: String::new(),
            per_side_size: String::new(),
            per_side_shares: String::new(),
            per_side_meets_min: true,
            min_size: String::new(),
            fill_risk: "high",
            other_leg: None,
            error: Some(message.into()),
        }
    }

    fn no_rewards() -> Self {
        Self {
            has_rewards: false,
            cents_from_mid: String::new(),
            per_side_size: String::new(),
            per_side_shares: String::new(),
            per_side_meets_min: true,
            min_size: String::new(),
            fill_risk: "high",
            other_leg: None,
            error: None,
        }
    }
}

/// POST /markets/view/preview — htmx live feedback as price/size change
/// (mirrors `setup::detect_wallet`'s as-you-type pattern).
pub async fn view_preview(Form(form): Form<PlacementPreviewForm>) -> Html<String> {
    let render = |tpl: PlacementPreviewTemplate| {
        Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
    };

    let price: Decimal = match form.price.trim().parse() {
        Ok(p) => p,
        Err(_) => return render(PlacementPreviewTemplate::error("Enter a valid price.")),
    };
    let order_size: Decimal = match form.order_size.trim().parse() {
        Ok(s) => s,
        Err(_) => return render(PlacementPreviewTemplate::error("Enter a valid order size.")),
    };

    let mr = match tokio::time::timeout(NETWORK_TIMEOUT, gamma_resolve::market_by_slug(&form.slug)).await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return render(PlacementPreviewTemplate::error(format!("Could not load market: {e}"))),
        Err(_) => return render(PlacementPreviewTemplate::error("Request to Polymarket timed out — try again.")),
    };
    if !mr.has_rewards() {
        return render(PlacementPreviewTemplate::no_rewards());
    }

    let side = form.side.min(mr.outcomes.len().saturating_sub(1));
    let token = &mr.token_ids[side];
    let both_sides = form.sides.as_deref() == Some("both");
    let max_spread_cents = mr.rewards_max_spread.unwrap_or(dec!(0));
    let min_size = mr.rewards_min_size.unwrap_or(dec!(0));

    let midpoint = match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_midpoint(token)).await {
        Ok(Ok(m)) => m,
        _ => mr.outcome_prices.get(side).copied().unwrap_or(dec!(0.5)),
    };
    let book = match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(token)).await {
        Ok(Ok(b)) => b,
        _ => return render(PlacementPreviewTemplate::error("Order book unavailable right now — try again.")),
    };

    let eval = market_data::evaluate_placement(&book, midpoint, max_spread_cents, min_size, price, order_size, both_sides);

    // Both-sides transparency: the other leg is derived — it rests at the SAME
    // distance below its own best bid, so surface its price + qualify rather
    // than leaving it invisible. (One set of params, applied symmetrically.)
    let other_leg = if both_sides {
        let other = 1 - side;
        let other_token = &mr.token_ids[other];
        let distance = book.best_bid.map(|bb| bb - price);
        let other_book = tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(other_token))
            .await.ok().and_then(Result::ok);
        let other_mid = tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_midpoint(other_token))
            .await.ok().and_then(Result::ok)
            .unwrap_or_else(|| mr.outcome_prices.get(other).copied().unwrap_or(dec!(0.5)));
        match (distance, other_book) {
            (Some(d), Some(ob)) if ob.best_bid.is_some() => {
                let op = ob.best_bid.unwrap() - d;
                let oe = market_data::evaluate_placement(&ob, other_mid, max_spread_cents, min_size, op, order_size, true);
                Some(OtherLeg {
                    label: mr.outcomes.get(other).cloned().unwrap_or_default(),
                    price_cents: format!("{:.1}", op * dec!(100)),
                    qualifies: oe.qualifies,
                    meets_min: oe.per_side_meets_min,
                    shares: format!("{:.0}", oe.per_side_shares),
                })
            }
            _ => None,
        }
    } else {
        None
    };

    render(PlacementPreviewTemplate {
        has_rewards: true,
        cents_from_mid: format!("{:.1}", eval.cents_from_mid),
        per_side_size: format!("${:.2}", eval.per_side_size),
        per_side_shares: format!("{:.0}", eval.per_side_shares),
        per_side_meets_min: eval.per_side_meets_min,
        min_size: min_size.normalize().to_string(),
        fill_risk: eval.fill_risk.label(),
        other_leg,
        error: None,
    })
}

// ── Start farming (creates 1 or 2 MarketConfigs from the visual placement) ──

#[derive(Deserialize)]
pub struct StartFarmingForm {
    csrf: String,
    slug: String,
    #[serde(default)]
    side: usize,
    sides: Option<String>,
    price: String,
    order_size: String,
    min_depth_cents: String,
    expires_in: String,
    max_volatility_cents: String,
}

#[derive(Template)]
#[template(path = "_start_farming_result.html")]
struct StartFarmingResultTemplate {
    error: Option<String>,
}

fn render_start_error(message: &str) -> Response {
    let tpl = StartFarmingResultTemplate { error: Some(message.to_string()) };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>"))).into_response()
}

/// POST /markets/start — turns the visual placement into 1 (one side) or 2
/// (both sides, sharing `condition_id`, size split in half) `MarketConfig`s.
/// Same write-through + rollback-on-save-failure discipline as `add_market`.
pub async fn start_farming(
    State(state): State<WebState>,
    session: Session,
    Form(form): Form<StartFarmingForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return render_start_error("Invalid session — retry.");
    }

    let price: Decimal = match form.price.trim().parse() {
        Ok(p) if p > dec!(0) && p < dec!(1) => p,
        _ => return render_start_error("Enter a valid price between 0 and 1."),
    };
    let order_size: Decimal = match form.order_size.trim().parse() {
        Ok(s) if s > dec!(0) => s,
        _ => return render_start_error("Order size must be a positive number."),
    };
    let min_depth_between = match parse_cents(&form.min_depth_cents) {
        Some(d) => d,
        None => return render_start_error("Min depth must be a number (in cents)."),
    };
    // Auto-pause is enabled by filling the threshold field; blank = disabled.
    let max_volatility = if form.max_volatility_cents.trim().is_empty() {
        None
    } else {
        match parse_cents(&form.max_volatility_cents) {
            Some(v) if v > dec!(0) => Some(v),
            _ => return render_start_error("Auto-pause threshold must be a positive number (in cents)."),
        }
    };
    let expires_at = match parse_expiry(&form.expires_in) {
        Ok(e) => e,
        Err(e) => return render_start_error(&e),
    };

    let mr = match tokio::time::timeout(NETWORK_TIMEOUT, gamma_resolve::market_by_slug(&form.slug)).await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return render_start_error(&format!("Could not load market: {e}")),
        Err(_) => return render_start_error("Request to Polymarket timed out — try again."),
    };
    let side = form.side.min(mr.outcomes.len().saturating_sub(1));
    let both_sides = form.sides.as_deref() == Some("both");

    let book_a = match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(&mr.token_ids[side])).await {
        Ok(Ok(b)) => b,
        _ => return render_start_error("Could not load the order book — try again."),
    };
    let best_bid_a = match book_a.best_bid {
        Some(b) => b,
        None => return render_start_error("No live bids on this market right now."),
    };
    if price >= best_bid_a {
        return render_start_error("Your price must be below the current best bid.");
    }
    // Both sides share this same distance — applied symmetrically to the
    // other leg's own best bid below (the locked "one set of params" design).
    let distance = best_bid_a - price;
    let per_side_size = if both_sides { order_size / dec!(2) } else { order_size };

    let mut new_configs = vec![MarketConfig {
        id: new_config_id(),
        url: format!("https://polymarket.com/event/{}", mr.market_slug),
        label: mr.question.clone(),
        condition_id: mr.condition_id.clone(),
        token_id: mr.token_ids[side].clone(),
        token_label: mr.outcomes[side].clone(),
        tick_size: mr.tick_size,
        distance,
        min_depth_between,
        order_size: per_side_size,
        expires_at,
        paused: false,
        benchmark_bid: Some(best_bid_a),
        max_volatility,
    }];

    if both_sides {
        let other = 1 - side;
        let book_b = match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(&mr.token_ids[other])).await {
            Ok(Ok(b)) => b,
            _ => return render_start_error("Could not load the order book for the other side — try again."),
        };
        let best_bid_b = match book_b.best_bid {
            Some(b) => b,
            None => return render_start_error("No live bids on the other side right now."),
        };
        new_configs.push(MarketConfig {
            id: new_config_id(),
            url: format!("https://polymarket.com/event/{}", mr.market_slug),
            label: mr.question.clone(),
            condition_id: mr.condition_id.clone(),
            token_id: mr.token_ids[other].clone(),
            token_label: mr.outcomes[other].clone(),
            tick_size: mr.tick_size,
            distance,
            min_depth_between,
            order_size: per_side_size,
            expires_at,
            paused: false,
            benchmark_bid: Some(best_bid_b),
            max_volatility,
        });
    }

    for cfg in &new_configs {
        if let Err(e) = cfg.validate() {
            return render_start_error(&e);
        }
    }

    let new_token_ids: Vec<String> = new_configs.iter().map(|c| c.token_id.clone()).collect();
    let (configs_snapshot, newly_subscribed, markets_file) = {
        let mut s = state.engine.write().await;
        let newly_subscribed: Vec<String> = new_token_ids
            .iter()
            .filter(|t| !s.configs.iter().any(|c| &c.token_id == *t))
            .cloned()
            .collect();
        for cfg in &new_configs {
            s.order_status.insert(cfg.id.clone(), OrderStatus::Idle);
        }
        s.configs.extend(new_configs.iter().cloned());
        (s.configs.clone(), newly_subscribed, s.markets_file.clone())
    };

    if let Err(e) = save_markets(&markets_file, &configs_snapshot) {
        // Roll back so disk and memory never diverge.
        let mut s = state.engine.write().await;
        let new_ids: Vec<&str> = new_configs.iter().map(|c| c.id.as_str()).collect();
        s.configs.retain(|c| !new_ids.contains(&c.id.as_str()));
        for cfg in &new_configs {
            s.order_status.remove(&cfg.id);
        }
        return render_start_error(&format!("Failed to save markets.json: {e}"));
    }

    if !newly_subscribed.is_empty() {
        if let Some((_, ws_cmd_tx)) = state.engine_handle.get().await {
            let _ = ws_cmd_tx.send(WsCommand::Subscribe(newly_subscribed)).await;
        }
        // Engine not running yet: no-op by design — ws_manager re-derives its
        // subscription set fresh from AppState.configs on its first connect.
    }

    let mut resp = Html(String::new()).into_response();
    resp.headers_mut().insert("HX-Redirect", "/markets".parse().expect("static header value"));
    resp
}

fn new_config_id() -> String {
    format!("mar_{}_{}", Utc::now().timestamp(), rand_alpha(6))
}

// ── Browse reward-eligible markets ──────────────────────────────────────────

#[derive(Deserialize, Default, Clone)]
pub struct BrowseParams {
    #[serde(default)]
    pub q: String,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub cursor: Option<String>,
}

impl BrowseParams {
    fn sort_or_default(&self) -> String {
        self.sort.clone().unwrap_or_else(|| "rate_per_day".to_string())
    }
    fn dir_or_default(&self) -> String {
        self.dir.clone().unwrap_or_else(|| "DESC".to_string())
    }
}

#[derive(Template)]
#[template(path = "browse_markets.html")]
struct BrowsePageTemplate {
    q: String,
    sort: String,
    dir: String,
}

pub async fn browse_page(Query(params): Query<BrowseParams>) -> Html<String> {
    let tpl = BrowsePageTemplate {
        q: params.q.clone(),
        sort: params.sort_or_default(),
        dir: params.dir_or_default(),
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

pub struct BrowseRow {
    pub question: String,
    pub market_slug: String,
    pub daily_pool: String,
    pub min_size: String,
    pub max_spread: String,
    pub spread_now: String,
    pub qualifies_now: bool,
    pub volume_24hr: String,
    pub outcomes: Vec<String>,
    pub prices_cents: Vec<String>,
}

#[derive(Template)]
#[template(path = "_browse_results.html")]
struct BrowseResultsTemplate {
    rows: Vec<BrowseRow>,
    has_more: bool,
    next_cursor: String,
    q: String,
    sort: String,
    dir: String,
    error: Option<String>,
}

/// htmx target for the browse page's search/sort/pagination.
pub async fn browse_results(Query(params): Query<BrowseParams>) -> Html<String> {
    let query = markets_browse::BrowseQuery {
        q: (!params.q.is_empty()).then(|| params.q.clone()),
        order_by: Some(params.sort_or_default()),
        position: Some(params.dir_or_default()),
        page_size: Some(25),
        next_cursor: params.cursor.clone(),
    };

    let outcome = tokio::time::timeout(NETWORK_TIMEOUT, markets_browse::browse(&query)).await;
    let (rows, has_more, next_cursor, error) = match outcome {
        Ok(Ok(resp)) => {
            let next_cursor = resp.next_cursor.unwrap_or_default();
            let has_more = !next_cursor.is_empty() && next_cursor != TERMINAL_CURSOR;
            (resp.data.into_iter().map(to_browse_row).collect(), has_more, next_cursor, None)
        }
        Ok(Err(e)) => (Vec::new(), false, String::new(), Some(format!("Could not load markets: {e}"))),
        Err(_) => (Vec::new(), false, String::new(), Some("Request timed out — try again.".to_string())),
    };

    let sort = params.sort_or_default();
    let dir = params.dir_or_default();
    let tpl = BrowseResultsTemplate {
        rows,
        has_more,
        next_cursor,
        q: params.q,
        sort,
        dir,
        error,
    };
    Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>")))
}

fn to_browse_row(m: markets_browse::RewardsMultiMarket) -> BrowseRow {
    let daily_pool: Decimal = m.rewards_config.iter().map(|c| c.rate_per_day).sum();
    let qualifies_now = m.spread <= m.rewards_max_spread;
    let (outcomes, prices_cents) =
        m.tokens.into_iter().map(|t| (t.outcome, fmt_cents(t.price))).unzip();
    BrowseRow {
        question: m.question,
        market_slug: m.market_slug,
        daily_pool: format!("${daily_pool}/day"),
        min_size: format!("${}", m.rewards_min_size),
        max_spread: format!("{:.1}c", m.rewards_max_spread),
        spread_now: format!("{:.1}c", m.spread),
        qualifies_now,
        volume_24hr: m.volume_24hr.map(|v| format!("${v:.0}")).unwrap_or_else(|| "—".to_string()),
        outcomes,
        prices_cents,
    }
}

// ── Resolve a pasted URL → straight to the view, or a picker for events
// with more than one tradeable sub-market ──────────────────────────────────

#[derive(Deserialize)]
pub struct ResolveUrlParams {
    #[serde(default)]
    pub url: String,
}

pub struct PickerCandidate {
    pub market_slug: String,
    pub label: String,
    pub image: Option<String>,
    pub outcomes: Vec<String>,
    pub prices_cents: Vec<String>,
    pub has_rewards: bool,
}

#[derive(Template)]
#[template(path = "market_picker.html")]
struct MarketPickerTemplate {
    candidates: Vec<PickerCandidate>,
}

/// GET /markets/resolve?url=&hellip; — paste-a-URL entry point. A market-slug
/// URL (or an event with exactly one tradeable market) goes straight to the
/// detail view; an event with several candidates (e.g. "World Cup Winner")
/// shows a picker instead of guessing which one was meant.
pub async fn resolve_url(Query(p): Query<ResolveUrlParams>) -> Response {
    let url = p.url.trim();
    if url.is_empty() {
        return view_error("Paste a Polymarket market or event URL first.");
    }

    match tokio::time::timeout(NETWORK_TIMEOUT, gamma_resolve::resolve_url(url)).await {
        Ok(Ok(gamma_resolve::Resolved::Single(mr))) => {
            Redirect::to(&format!("/markets/view?slug={}", mr.market_slug)).into_response()
        }
        Ok(Ok(gamma_resolve::Resolved::Multiple(refs))) => {
            let candidates = refs.into_iter().map(to_picker_candidate).collect();
            let tpl = MarketPickerTemplate { candidates };
            Html(tpl.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>"))).into_response()
        }
        Ok(Err(e)) => view_error(&format!("Could not resolve this URL: {e}")),
        Err(_) => view_error("Request to Polymarket timed out — try again."),
    }
}

fn to_picker_candidate(mr: gamma_resolve::MarketRef) -> PickerCandidate {
    let has_rewards = mr.has_rewards();
    let prices_cents = mr.outcome_prices.iter().map(|p| fmt_cents(*p)).collect();
    PickerCandidate {
        market_slug: mr.market_slug,
        label: mr.group_item_title.unwrap_or(mr.question),
        image: mr.image,
        outcomes: mr.outcomes,
        prices_cents,
        has_rewards,
    }
}

/// "3.0" (cents) -> 0.03 (price-unit Decimal), matching how `MarketConfig`
/// stores `distance`/`min_depth_between` (the dashboard always displays/
/// accepts cents; the engine works in price units).
fn parse_cents(s: &str) -> Option<Decimal> {
    let cents: Decimal = s.trim().parse().ok()?;
    Some(cents / Decimal::from(100))
}

/// Parses "<number><unit>" (s/m/h/d), e.g. "7d", "4h", "30m" — same format
/// the original Discord bot used. Range: 1 minute .. 1 year. Empty -> 7 days.
fn parse_expiry(s: &str) -> Result<chrono::DateTime<Utc>, String> {
    let s = s.trim();
    // "never" = quote indefinitely — a far-future sentinel so the expiry check
    // never fires (no schema change to MarketConfig's non-optional expires_at).
    if s.eq_ignore_ascii_case("never") {
        return Ok(Utc::now() + chrono::Duration::days(36500));
    }
    let s = if s.is_empty() { "7d" } else { s };
    let (num_part, unit) = s.split_at(s.len() - 1);
    let n: i64 = num_part.parse().map_err(|_| format!("Invalid expiry '{s}' — use e.g. 7d, 4h, 30m."))?;
    let duration = match unit {
        "s" => chrono::Duration::seconds(n),
        "m" => chrono::Duration::minutes(n),
        "h" => chrono::Duration::hours(n),
        "d" => chrono::Duration::days(n),
        _ => return Err(format!("Invalid expiry unit in '{s}' — use s, m, h, or d.")),
    };
    if duration < chrono::Duration::minutes(1) || duration > chrono::Duration::days(366) {
        return Err("Expiry must be between 1 minute and 1 year.".to_string());
    }
    Ok(Utc::now() + duration)
}

fn rand_alpha(n: usize) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..n).map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char).collect()
}

// ── Remove / pause / resume ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MarketActionForm {
    csrf: String,
}

/// POST /markets/{cid}/remove — `cid` is a `condition_id`; removes every
/// config sharing it (one-side = a group of one, both-sides = two legs).
pub async fn remove_market(
    State(state): State<WebState>,
    session: Session,
    Path(cid): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return Html("Invalid session — retry.").into_response();
    }

    let (configs_snapshot, removed, orders_to_cancel, tokens_to_unsubscribe, markets_file) = {
        let mut s = state.engine.write().await;
        let removed: Vec<_> = s.configs.iter().filter(|c| c.condition_id == cid).cloned().collect();
        if removed.is_empty() {
            return Html("").into_response(); // already gone — nothing to do
        }
        let mut orders_to_cancel = Vec::new();
        for c in &removed {
            if let Some(OrderStatus::Live { order_id, .. }) | Some(OrderStatus::Cancelling { order_id, .. }) =
                s.order_status.get(&c.id)
            {
                orders_to_cancel.push(order_id.clone());
            }
            s.order_status.remove(&c.id);
            s.place_failures.remove(&c.id);
        }
        s.configs.retain(|c| c.condition_id != cid);
        let tokens_to_unsubscribe: Vec<String> = removed
            .iter()
            .map(|c| c.token_id.clone())
            .filter(|tid| !s.configs.iter().any(|c| &c.token_id == tid))
            .collect();
        (s.configs.clone(), removed, orders_to_cancel, tokens_to_unsubscribe, s.markets_file.clone())
    };

    if let Err(e) = save_markets(&markets_file, &configs_snapshot) {
        // Roll back.
        let mut s = state.engine.write().await;
        s.configs.extend(removed);
        tracing::error!("Failed to save markets.json after remove: {}", e);
        return Html(format!("Failed to save: {e}")).into_response();
    }

    if let Some((executor, ws_cmd_tx)) = state.engine_handle.get().await {
        if !orders_to_cancel.is_empty() {
            let _ = executor.cancel_orders(&orders_to_cancel).await;
        }
        if !tokens_to_unsubscribe.is_empty() {
            let _ = ws_cmd_tx.send(WsCommand::Unsubscribe(tokens_to_unsubscribe)).await;
        }
    }

    Html("").into_response()
}

/// POST /markets/{cid}/pause and /markets/{cid}/resume share this — `pausing`
/// picks the direction. `cid` is a `condition_id`; flips every leg sharing it
/// together, so a both-sides farm is always paused/resumed as one unit.
async fn set_paused(state: &WebState, cid: &str, pausing: bool) -> Result<(), String> {
    let (configs_snapshot, orders_to_cancel, markets_file) = {
        let mut s = state.engine.write().await;
        let ids: Vec<String> =
            s.configs.iter().filter(|c| c.condition_id == cid).map(|c| c.id.clone()).collect();
        if ids.is_empty() {
            return Ok(()); // already gone
        }
        for c in s.configs.iter_mut().filter(|c| c.condition_id == cid) {
            c.paused = pausing;
        }
        let orders_to_cancel: Vec<(String, String)> = if pausing {
            ids.iter()
                .filter_map(|id| match s.order_status.get(id) {
                    Some(OrderStatus::Live { order_id, .. }) => Some((id.clone(), order_id.clone())),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        (s.configs.clone(), orders_to_cancel, s.markets_file.clone())
    };

    if let Err(e) = save_markets(&markets_file, &configs_snapshot) {
        // Roll back the flag flip.
        let mut s = state.engine.write().await;
        for c in s.configs.iter_mut().filter(|c| c.condition_id == cid) {
            c.paused = !pausing;
        }
        return Err(format!("Failed to save: {e}"));
    }

    for (id, order_id) in orders_to_cancel {
        {
            let mut s = state.engine.write().await;
            s.order_status.insert(
                id.clone(),
                OrderStatus::Cancelling { order_id: order_id.clone(), since: std::time::Instant::now() },
            );
        }
        if let Some((executor, _)) = state.engine_handle.get().await {
            let confirmed = executor.cancel_order_verified(&order_id).await.unwrap_or(false);
            if confirmed {
                let mut s = state.engine.write().await;
                if matches!(s.order_status.get(&id), Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == &order_id)
                {
                    s.order_status.insert(id.clone(), OrderStatus::Idle);
                }
            }
            // If not confirmed: leave Cancelling — the existing 30s timeout
            // recovery in `evaluate_all_markets` already handles this case.
        }
    }

    Ok(())
}

pub async fn pause_market(
    State(state): State<WebState>,
    session: Session,
    Path(cid): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return Html("Invalid session — retry.").into_response();
    }
    match set_paused(&state, &cid, true).await {
        Ok(()) => Html("").into_response(),
        Err(e) => Html(e).into_response(),
    }
}

pub async fn resume_market(
    State(state): State<WebState>,
    session: Session,
    Path(cid): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return Html("Invalid session — retry.").into_response();
    }
    match set_paused(&state, &cid, false).await {
        Ok(()) => Html("").into_response(),
        Err(e) => Html(e).into_response(),
    }
}
