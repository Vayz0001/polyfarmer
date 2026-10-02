//! Market lifecycle: browse reward-eligible markets, resolve a pasted URL, the
//! interactive market view (live book, price chart, placement preview), and
//! add / edit / pause / resume / remove. Write-through — handlers mutate
//! `AppState` directly and persist atomically (rollback on save failure).

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use chrono::Utc;
use futures_util::stream::Stream;
use polymarket_client_sdk_v2::clob::types::Interval;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tower_sessions::Session;

use crate::cache::TtlCache;
use crate::rewards::gamma_resolve::{self, MarketRef};
use crate::rewards::{market_data, markets_browse};
use crate::storage::save_markets;
use crate::types::{MarketConfig, OrderStatus, WsCommand};

use super::auth::{csrf_token, verify_csrf};
use super::dashboard::{cents, render_markets_table, until};
use super::shell::{render, shell, Shell};
use super::state::WebState;

/// Hard upper bound on any handler-initiated network call — a request
/// handler must not hang on a flaky upstream.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(8);
/// `user_earnings_and_markets_config` scans every reward-eligible market server-
/// side (can't be scoped to one market) — give it a longer budget. Only runs
/// in the async Your-position fragment.
const POSITION_EARNINGS_TIMEOUT: Duration = Duration::from_secs(20);
/// Polymarket's pagination sentinel (base64 of "-1").
const TERMINAL_CURSOR: &str = "LTE=";
/// Book levels per side on the ladder (the ladder area scrolls).
const LADDER_LEVELS: usize = 50;
/// Sensible starting order size ($) prefilled in the placement panel.
const DEFAULT_ORDER_SIZE: &str = "100";
/// Live-book pushes are coalesced to at most one per this interval.
const BOOK_PUSH_MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Browse results (query → response). Polymarket's endpoint takes ~5–10s, so
/// results are served stale-while-revalidate: instant after the first load.
static BROWSE: LazyLock<Arc<TtlCache<Arc<markets_browse::RewardsMultiResponse>>>> =
    LazyLock::new(|| Arc::new(TtlCache::new(Duration::from_secs(60))));
const BROWSE_PAGE_SIZE: u32 = 30;

fn browse_key(q: &str, sort: &str, dir: &str, cursor: &str) -> String {
    format!("{q}|{sort}|{dir}|{cursor}")
}

/// Warm the default browse view (top daily pools) in the background so the
/// first visit to "Find markets" doesn't wait on Polymarket.
pub fn prewarm_browse() {
    tokio::spawn(async {
        let query = markets_browse::BrowseQuery {
            order_by: Some("rate_per_day".into()),
            position: Some("DESC".into()),
            page_size: Some(BROWSE_PAGE_SIZE),
            ..Default::default()
        };
        if let Ok(resp) = markets_browse::browse(&query).await {
            BROWSE.put(&browse_key("", "rate_per_day", "DESC", ""), Arc::new(resp));
        }
    });
}

/// Price history barely moves at chart resolution — cache per token+range.
static HISTORY: LazyLock<TtlCache<Vec<(i64, Decimal)>>> = LazyLock::new(|| TtlCache::new(Duration::from_secs(60)));

async fn load_market(slug: &str) -> Result<MarketRef, String> {
    match tokio::time::timeout(NETWORK_TIMEOUT, gamma_resolve::market_by_slug_cached(slug)).await {
        Ok(Ok(m)) => Ok(m),
        Ok(Err(e)) => Err(format!("Could not load this market: {e}")),
        Err(_) => Err("Request to Polymarket timed out — try again.".to_string()),
    }
}

/// The live (WS-fed) book when the hub has it, else a REST snapshot.
async fn current_book(state: &WebState, mr: &MarketRef, side: usize) -> Option<market_data::BookSnapshot> {
    let token = &mr.token_ids[side];
    if let Some(b) = state.book_hub.snapshot(token, mr.tick_size) {
        return Some(b);
    }
    tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(token)).await.ok().and_then(Result::ok)
}

/// Midpoint from the book's top; REST midpoint / Gamma price as fallbacks.
async fn midpoint_for(mr: &MarketRef, side: usize, book: Option<&market_data::BookSnapshot>) -> Decimal {
    if let Some(m) = book.and_then(market_data::book_midpoint) {
        return m;
    }
    match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_midpoint(&mr.token_ids[side])).await {
        Ok(Ok(m)) => m,
        _ => mr.outcome_prices.get(side).copied().unwrap_or(dec!(0.5)),
    }
}

/// Prices of your live engine orders resting on `token` (for ladder markers).
async fn live_mine(state: &WebState, token: &str) -> Vec<Decimal> {
    let s = state.engine.read().await;
    s.configs
        .iter()
        .filter(|c| c.token_id == token)
        .filter_map(|c| match s.order_status.get(&c.id) {
            Some(OrderStatus::Live { price, .. }) => Some(*price),
            _ => None,
        })
        .collect()
}

// ── Interactive market view ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ViewParams {
    pub slug: String,
    /// Which outcome (0 or 1) is selected. Defaults to 0.
    #[serde(default)]
    pub side: usize,
    /// "one" | "both" — farm one outcome or both legs. Defaults to "one".
    pub sides: Option<String>,
}

pub struct PresetView {
    pub key: &'static str,
    pub label: &'static str,
    pub price_cents: String,
    pub weight_pct: String,
}

/// One outcome's pre-rendered data. Both sides are embedded so switching the
/// outcome tab is a client-side swap; the live book re-streams for that side.
pub struct SideView {
    pub idx: usize,
    pub label: String,
    pub price_cents: String,
    pub presets: Vec<PresetView>,
    pub selected: bool,
}

/// An order-book price-grouping choice (raw tick + coarser cent buckets).
pub struct GroupOpt {
    pub value: String, // group size in price units, e.g. "0.001"
    pub label: String, // "0.1¢" / "1¢" / "5¢"
    pub selected: bool,
}

/// Grouping choices: the market's raw tick plus up to two coarser cent buckets.
fn group_options(tick: Decimal) -> Vec<GroupOpt> {
    let tick_cents = tick * dec!(100);
    let mut cs: Vec<Decimal> = vec![tick_cents];
    for c in [dec!(1), dec!(5), dec!(10)] {
        if c > tick_cents {
            cs.push(c);
        }
    }
    cs.truncate(3);
    cs.into_iter()
        .enumerate()
        .map(|(i, c)| GroupOpt {
            value: (c / dec!(100)).normalize().to_string(),
            label: format!("{}\u{00a2}", c.normalize()),
            selected: i == 0,
        })
        .collect()
}

fn preset_views(mid: Decimal, max_spread: Decimal, tick: Decimal, best_bid: Option<Decimal>) -> Vec<PresetView> {
    market_data::placement_presets(mid, max_spread, tick, best_bid)
        .into_iter()
        .map(|p| PresetView {
            key: p.key,
            label: p.label,
            price_cents: (p.price * dec!(100)).normalize().to_string(),
            weight_pct: super::dashboard::pct(p.weight),
        })
        .collect()
}

#[derive(Template)]
#[template(path = "market_view.html")]
struct MarketViewTemplate {
    shell: Shell,
    csrf_token: String,
    slug: String,
    url: String,
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
    farming: bool,
    sides: Vec<SideView>,
    selected_idx: usize,
    selected_label: String,
    group_opts: Vec<GroupOpt>,
    sides_both: bool,
    ladder: market_data::Ladder,
    default_size: String,
    /// Default price (¢) prefilled: the Balanced preset, else the first one.
    default_price_cents: String,
    /// Tick in cents (input step/min) and the highest valid price in cents.
    tick_cents: String,
    price_max_cents: String,
    /// Reward minimum in SHARES ("" if no rewards) and max spread in cents
    /// ("0" if none) — feed the instant client-side score calc.
    reward_min_shares: String,
    reward_max_spread_cents: String,
}

#[derive(Template)]
#[template(path = "market_view_error.html")]
struct MarketViewErrorTemplate {
    shell: Shell,
    message: String,
}

async fn view_error(session: &Session, message: &str) -> Response {
    render(&MarketViewErrorTemplate { shell: shell(session, "markets").await, message: message.to_string() }).into_response()
}

/// GET /markets/view?slug=&side=&sides= — the trading-terminal-style view.
/// First paint needs only cached metadata + one book read; the price chart and
/// your position load lazily, and the book then streams live.
pub async fn market_view(State(state): State<WebState>, session: Session, Query(p): Query<ViewParams>) -> Response {
    let mr = match load_market(&p.slug).await {
        Ok(m) => m,
        Err(e) => return view_error(&session, &e).await,
    };
    let side = p.side.min(mr.outcomes.len().saturating_sub(1));
    let max_spread = mr.rewards_max_spread.unwrap_or(dec!(0));
    let book = current_book(&state, &mr, side).await;
    let mid = midpoint_for(&mr, side, book.as_ref()).await;
    let (bb, ba) = book.as_ref().map(|b| (b.best_bid, b.best_ask)).unwrap_or((None, None));

    // Binary books mirror: the other outcome's bid = 1 − this ask, mid = 1 − mid.
    let side_data = |i: usize| -> (Decimal, Option<Decimal>) {
        if i == side { (mid, bb) } else { (dec!(1) - mid, ba.map(|a| dec!(1) - a)) }
    };
    let sides: Vec<SideView> = mr
        .outcomes
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let (m, b) = side_data(i);
            SideView {
                idx: i,
                label: label.clone(),
                price_cents: mr.outcome_prices.get(i).map(|p| cents(*p)).unwrap_or_else(|| "—".to_string()),
                presets: preset_views(m, max_spread, mr.tick_size, b),
                selected: i == side,
            }
        })
        .collect();
    let default_price_cents = sides[side]
        .presets
        .iter()
        .find(|p| p.key == "balanced")
        .or_else(|| sides[side].presets.first())
        .map(|p| p.price_cents.clone())
        .unwrap_or_default();

    let ladder = match &book {
        Some(b) => market_data::build_ladder(b, mid, max_spread, LADDER_LEVELS, &live_mine(&state, &mr.token_ids[side]).await, mr.tick_size),
        None => empty_ladder(mid),
    };

    let now = Utc::now();
    let farming = state.engine.read().await.configs.iter().any(|c| c.condition_id == mr.condition_id);
    let tpl = MarketViewTemplate {
        shell: shell(&session, "markets").await,
        csrf_token: csrf_token(&session).await,
        slug: p.slug.clone(),
        url: poly_event_url(&mr.event_slug, &mr.market_slug),
        condition_id: mr.condition_id.clone(),
        question: mr.question.clone(),
        image: mr.image.clone().filter(|s| !s.is_empty()),
        group_item_title: mr.group_item_title.clone().filter(|g| !g.is_empty()),
        has_rewards: mr.has_rewards(),
        reward_min: mr.rewards_min_size.map(|s| format!("{} shares", s.normalize())).unwrap_or_else(|| "—".to_string()),
        reward_max_spread: mr.rewards_max_spread.map(|s| format!("±{}¢", s.normalize())).unwrap_or_else(|| "—".to_string()),
        volume_24hr: mr.volume_24hr.filter(|v| *v > dec!(0)).map(market_data::fmt_usd),
        liquidity: mr.liquidity.filter(|l| *l > dec!(0)).map(market_data::fmt_usd),
        resolves: mr.end_date.map(|d| d.format("%b %-d, %Y").to_string()),
        countdown: mr.end_date.map(|d| {
            let days = (d - now).num_days();
            if days > 1 { format!("{days}d left") } else if days == 1 { "1d left".into() } else if days == 0 { "ends today".into() } else { "ended".into() }
        }),
        farming,
        selected_label: mr.outcomes.get(side).cloned().unwrap_or_default(),
        group_opts: group_options(mr.tick_size),
        sides_both: p.sides.as_deref() == Some("both"),
        sides,
        selected_idx: side,
        ladder,
        default_size: DEFAULT_ORDER_SIZE.to_string(),
        default_price_cents,
        tick_cents: (mr.tick_size * dec!(100)).normalize().to_string(),
        price_max_cents: ((dec!(1) - mr.tick_size) * dec!(100)).normalize().to_string(),
        reward_min_shares: mr.rewards_min_size.map(|s| s.normalize().to_string()).unwrap_or_default(),
        reward_max_spread_cents: mr.rewards_max_spread.map(|s| s.normalize().to_string()).unwrap_or_else(|| "0".to_string()),
    };
    render(&tpl).into_response()
}

fn empty_ladder(mid: Decimal) -> market_data::Ladder {
    market_data::Ladder {
        asks: vec![],
        bids: vec![],
        midpoint_cents: cents(mid),
        spread_cents: "—".to_string(),
        best_bid_cents: "—".to_string(),
        best_ask_cents: "—".to_string(),
        band_lo_cents: "—".to_string(),
        band_hi_cents: "—".to_string(),
        in_zone_usdc: "—".to_string(),
        in_zone_raw: "0".to_string(),
        best_bid_raw: String::new(),
        best_ask_raw: String::new(),
        midpoint_raw: mid.normalize().to_string(),
        has_book: false,
    }
}

// ── Price chart (lazy fragment) ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ChartParams {
    pub slug: String,
    #[serde(default)]
    pub side: usize,
    /// "1d" | "1w" | "all"
    #[serde(default)]
    pub range: String,
}

#[derive(Template)]
#[template(path = "_chart.html")]
struct ChartTemplate {
    svg: String,
    caption: String,
    hi: String,
    lo: String,
}

/// GET /markets/view/chart?slug=&side=&range=
pub async fn view_chart(Query(p): Query<ChartParams>) -> Html<String> {
    let (interval, label) = match p.range.as_str() {
        "1d" => (Interval::OneDay, "24h"),
        "all" => (Interval::Max, "All-time"),
        _ => (Interval::OneWeek, "1W"),
    };
    let points = match load_market(&p.slug).await {
        Ok(mr) => {
            let token = mr.token_ids[p.side.min(1)].clone();
            let key = format!("{token}:{}", p.range);
            HISTORY
                .get_or_fetch(&key, || async {
                    tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_history(&token, interval))
                        .await
                        .map_err(|_| eyre::eyre!("timeout"))?
                })
                .await
                .unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };
    let (lo, hi) = match market_data::history_range(&points) {
        Some((lo, hi, _)) => (format!("{lo}¢"), format!("{hi}¢")),
        None => (String::new(), String::new()),
    };
    render(&ChartTemplate {
        svg: market_data::price_chart_svg(&points),
        caption: market_data::chart_caption(&points, label),
        hi,
        lo,
    })
}

// ── Order book: REST fragment (fallback) + live SSE stream ──────────────────

#[derive(Deserialize)]
pub struct BookParams {
    pub slug: String,
    #[serde(default)]
    pub side: usize,
    /// Price-bucket size for the grouping control (defaults to the raw tick).
    pub group: Option<String>,
}

#[derive(Template)]
#[template(path = "_book_ladder.html")]
struct BookLadderTemplate {
    ladder: market_data::Ladder,
}

fn parse_group(raw: Option<&str>, tick: Decimal) -> Decimal {
    raw.and_then(|s| s.trim().parse::<Decimal>().ok()).filter(|g| *g > dec!(0)).unwrap_or(tick)
}

async fn render_ladder(state: &WebState, mr: &MarketRef, side: usize, group: Decimal, book: &market_data::BookSnapshot) -> String {
    let mid = midpoint_for(mr, side, Some(book)).await;
    let max_spread = mr.rewards_max_spread.unwrap_or(dec!(0));
    let mine = live_mine(state, &mr.token_ids[side]).await;
    let ladder = market_data::build_ladder(book, mid, max_spread, LADDER_LEVELS, &mine, group);
    BookLadderTemplate { ladder }.render().unwrap_or_else(|e| format!("<pre>template error: {e}</pre>"))
}

/// GET /markets/view/book — one-shot ladder render (used when the live stream
/// is unavailable, and when switching grouping before the stream reconnects).
pub async fn view_book(State(state): State<WebState>, Query(p): Query<BookParams>) -> Html<String> {
    let Ok(mr) = load_market(&p.slug).await else {
        return render(&BookLadderTemplate { ladder: empty_ladder(dec!(0.5)) });
    };
    let side = p.side.min(mr.outcomes.len().saturating_sub(1));
    let group = parse_group(p.group.as_deref(), mr.tick_size);
    match current_book(&state, &mr, side).await {
        Some(book) => Html(render_ladder(&state, &mr, side, group, &book).await),
        None => render(&BookLadderTemplate { ladder: empty_ladder(midpoint_for(&mr, side, None).await) }),
    }
}

/// GET /markets/view/book/stream — Server-Sent Events: a freshly rendered
/// ladder (`book` event) whenever the live book changes, coalesced to ≤4/s.
/// Holding the stream holds the token's subscription in the book hub.
pub async fn view_book_stream(
    State(state): State<WebState>,
    Query(p): Query<BookParams>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    struct Ctx {
        state: WebState,
        mr: Option<Arc<MarketRef>>,
        side: usize,
        group: Decimal,
        rx: tokio::sync::broadcast::Receiver<String>,
        _guard: Option<super::book_hub::BookGuard>,
        dirty: bool,
        last_sent: Option<Instant>,
    }
    let mr = load_market(&p.slug).await.ok().map(Arc::new);
    let side = mr.as_ref().map(|m| p.side.min(m.outcomes.len().saturating_sub(1))).unwrap_or(0);
    let group = mr.as_ref().map(|m| parse_group(p.group.as_deref(), m.tick_size)).unwrap_or(dec!(0.01));
    let rx = state.book_hub.updates();
    let guard = mr.as_ref().map(|m| state.book_hub.acquire(&m.token_ids[side]));
    let ctx = Ctx { state, mr, side, group, rx, _guard: guard, dirty: true, last_sent: None };

    let stream = futures_util::stream::unfold(ctx, |mut ctx| async move {
        let mr = ctx.mr.clone()?; // unknown market → end the stream
        let token = mr.token_ids[ctx.side].clone();
        loop {
            if ctx.dirty {
                let wait = ctx
                    .last_sent
                    .map(|t| BOOK_PUSH_MIN_INTERVAL.saturating_sub(t.elapsed()))
                    .unwrap_or_default();
                if wait.is_zero() {
                    ctx.dirty = false;
                    if let Some(book) = ctx.state.book_hub.snapshot(&token, mr.tick_size) {
                        ctx.last_sent = Some(Instant::now());
                        let html = render_ladder(&ctx.state, &mr, ctx.side, ctx.group, &book).await;
                        return Some((Ok(Event::default().event("book").data(html)), ctx));
                    }
                    continue; // snapshot not here yet — wait for the next update
                }
                tokio::select! {
                    r = ctx.rx.recv() => match r {
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => return None,
                    },
                    _ = tokio::time::sleep(wait) => {}
                }
                continue;
            }
            match ctx.rx.recv().await {
                Ok(t) if t == token => ctx.dirty = true,
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => ctx.dirty = true,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── Your position (authenticated; only when you're farming this market) ─────

#[derive(Deserialize)]
pub struct PositionParams {
    pub cid: String,
}

pub struct PositionLeg {
    pub id: String,
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

/// GET /markets/view/position?cid= — loaded async so the heavier
/// authenticated reward call never blocks the market page. Renders only when
/// the engine is running AND you're farming this market.
pub async fn view_position(State(state): State<WebState>, Query(p): Query<PositionParams>) -> Html<String> {
    let (legs_cfg, order_status) = {
        let s = state.engine.read().await;
        let legs: Vec<MarketConfig> = s.configs.iter().filter(|c| c.condition_id == p.cid).cloned().collect();
        (legs, s.order_status.clone())
    };
    if legs_cfg.is_empty() {
        return render(&YourPositionTemplate::hidden());
    }
    let Some((executor, _)) = state.engine_handle.get().await else {
        return render(&YourPositionTemplate::hidden());
    };

    let live_ids: Vec<String> = legs_cfg
        .iter()
        .filter_map(|c| match order_status.get(&c.id) {
            Some(OrderStatus::Live { order_id, .. }) => Some(order_id.clone()),
            _ => None,
        })
        .collect();
    let scoring_fut = async {
        if live_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            let refs: Vec<&str> = live_ids.iter().map(String::as_str).collect();
            tokio::time::timeout(NETWORK_TIMEOUT, executor.are_orders_scoring(&refs))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default()
        }
    };
    let earn_fut = tokio::time::timeout(
        POSITION_EARNINGS_TIMEOUT,
        executor.user_earnings_and_markets_config(Utc::now().date_naive()),
    );
    let (scoring, earn_res) = tokio::join!(scoring_fut, earn_fut);

    let (reward_pct, earnings_today) = match earn_res {
        Ok(Ok(entries)) => entries
            .iter()
            .find(|e| e.condition_id.to_string().eq_ignore_ascii_case(&p.cid))
            .map(|e| {
                let earned: Decimal = e.earnings.iter().map(|a| a.earnings).sum();
                (Some(format!("{:.2}%", e.earning_percentage)), Some(format!("${earned:.2}")))
            })
            .unwrap_or((None, None)),
        _ => (None, None),
    };

    let legs: Vec<PositionLeg> = legs_cfg
        .iter()
        .map(|c| {
            let (status, scoring_leg) = match order_status.get(&c.id) {
                _ if c.paused => ("paused".to_string(), None),
                Some(OrderStatus::Live { order_id, price }) => (format!("resting {}¢", cents(*price)), scoring.get(order_id).copied()),
                Some(OrderStatus::Placing { .. }) => ("placing…".to_string(), None),
                Some(OrderStatus::Cancelling { .. }) => ("cancelling…".to_string(), None),
                _ => ("waiting for conditions".to_string(), None),
            };
            PositionLeg { id: c.id.clone(), label: c.token_label.clone(), status, scoring: scoring_leg }
        })
        .collect();

    let scoring_any = legs.iter().any(|l| l.scoring == Some(true));
    render(&YourPositionTemplate { show: true, scoring_any, reward_pct, earnings_today, legs })
}

// ── Live placement preview ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PlacementPreviewForm {
    slug: String,
    #[serde(default)]
    side: usize,
    sides: Option<String>,
    price_cents: String,
    order_size: String,
}

/// In both-sides mode the other leg is DERIVED: it rests the same distance
/// below *its* best bid. Shown for transparency (you only set one price).
struct OtherLeg {
    label: String,
    price_cents: String,
    weight_pct: String,
    meets_min: bool,
    shares: String,
}

#[derive(Template)]
#[template(path = "_placement_preview.html")]
struct PlacementPreviewTemplate {
    has_rewards: bool,
    cents_from_mid: String,
    below_bid: Option<String>,
    weight_pct: String,
    effective_pct: String,
    /// "ok" | "warn" | "bad"
    effective_tone: &'static str,
    sided_note: String,
    per_side_size: String,
    per_side_shares: String,
    per_side_meets_min: bool,
    min_size: String,
    fill_risk: &'static str,
    other_leg: Option<OtherLeg>,
    error: Option<String>,
}

impl PlacementPreviewTemplate {
    fn message(has_rewards: bool, error: Option<String>) -> Self {
        Self {
            has_rewards,
            cents_from_mid: String::new(),
            below_bid: None,
            weight_pct: String::new(),
            effective_pct: String::new(),
            effective_tone: "bad",
            sided_note: String::new(),
            per_side_size: String::new(),
            per_side_shares: String::new(),
            per_side_meets_min: true,
            min_size: String::new(),
            fill_risk: "high",
            other_leg: None,
            error,
        }
    }
}

fn pct(w: Decimal) -> String {
    super::dashboard::pct(w)
}

/// POST /markets/view/preview — server-side placement feedback (fill risk,
/// both-sides derivation, true reward weight), debounced as you type. Reads
/// the hub's live book, so it costs no Polymarket calls once streaming.
pub async fn view_preview(State(state): State<WebState>, Form(form): Form<PlacementPreviewForm>) -> Html<String> {
    let price = match form.price_cents.trim().parse::<Decimal>() {
        Ok(c) if c > dec!(0) && c < dec!(100) => c / dec!(100),
        _ => return render(&PlacementPreviewTemplate::message(true, Some("Enter a price between 0 and 100¢.".into()))),
    };
    let order_size: Decimal = match form.order_size.trim().parse() {
        Ok(s) if s > dec!(0) => s,
        _ => return render(&PlacementPreviewTemplate::message(true, Some("Enter a positive order size.".into()))),
    };
    let mr = match load_market(&form.slug).await {
        Ok(m) => m,
        Err(e) => return render(&PlacementPreviewTemplate::message(true, Some(e))),
    };
    if !mr.has_rewards() {
        return render(&PlacementPreviewTemplate::message(false, None));
    }
    let side = form.side.min(mr.outcomes.len().saturating_sub(1));
    let both_sides = form.sides.as_deref() == Some("both");
    let max_spread = mr.rewards_max_spread.unwrap_or(dec!(0));
    let min_size = mr.rewards_min_size.unwrap_or(dec!(0));

    let Some(book) = current_book(&state, &mr, side).await else {
        return render(&PlacementPreviewTemplate::message(true, Some("Order book unavailable right now — try again.".into())));
    };
    let mid = midpoint_for(&mr, side, Some(&book)).await;
    let eval = market_data::evaluate_placement(&book, mid, max_spread, min_size, price, order_size, both_sides);
    // An order under the size minimum doesn't score at all.
    let this_w = if eval.per_side_meets_min { eval.score_weight } else { dec!(0) };

    let other_leg = if both_sides {
        // Binary books mirror: other bid = 1 − this best ask.
        match (book.best_bid, book.best_ask) {
            (Some(bb), Some(ba)) => {
                let distance = bb - price;
                let op = (dec!(1) - ba) - distance;
                let other_mid = dec!(1) - mid;
                let shares = if op > dec!(0) { (eval.per_side_size / op).round_dp(0) } else { dec!(0) };
                let meets = shares >= min_size;
                let w = if op > dec!(0) && meets { market_data::score_weight(other_mid, op, max_spread) } else { dec!(0) };
                Some((OtherLeg {
                    label: mr.outcomes.get(1 - side).cloned().unwrap_or_default(),
                    price_cents: cents(op),
                    weight_pct: pct(w),
                    meets_min: meets,
                    shares: format!("{shares}"),
                }, w))
            }
            _ => None,
        }
    } else {
        None
    };

    let effective = market_data::effective_weight(mid, this_w, other_leg.as_ref().map(|(_, w)| *w));
    let sided_note = if !both_sides {
        if market_data::single_sided_allowed(mid) {
            "One-sided scores ÷3 — farm both sides for full weight".to_string()
        } else {
            "One-sided earns nothing while the midpoint is under 10¢ or over 90¢ — switch to Both sides".to_string()
        }
    } else if market_data::single_sided_allowed(mid) {
        "Two-sided — the weaker leg sets your score (or a third of the stronger)".to_string()
    } else {
        "Two-sided required here — the weaker leg sets your score".to_string()
    };
    let effective_tone = if effective >= dec!(0.4) {
        "ok"
    } else if effective > dec!(0) {
        "warn"
    } else {
        "bad"
    };

    render(&PlacementPreviewTemplate {
        has_rewards: true,
        cents_from_mid: format!("{:.1}", eval.cents_from_mid),
        below_bid: book.best_bid.map(|bb| format!("{:.1}", (bb - price) * dec!(100))),
        weight_pct: pct(this_w),
        effective_pct: pct(effective),
        effective_tone,
        sided_note,
        per_side_size: format!("${:.2}", eval.per_side_size),
        per_side_shares: format!("{:.0}", eval.per_side_shares),
        per_side_meets_min: eval.per_side_meets_min,
        min_size: min_size.normalize().to_string(),
        fill_risk: eval.fill_risk.label(),
        other_leg: other_leg.map(|(o, _)| o),
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
    price_cents: String,
    order_size: String,
    #[serde(default)]
    min_depth_usd: String,
    #[serde(default)]
    expires_in: String,
    #[serde(default)]
    max_volatility_cents: String,
}

#[derive(Template)]
#[template(path = "_start_farming_result.html")]
struct StartFarmingResultTemplate {
    error: Option<String>,
}

fn render_start_error(message: &str) -> Response {
    render(&StartFarmingResultTemplate { error: Some(message.to_string()) }).into_response()
}

/// Min depth ahead of the order, entered in **USD** — `MarketConfig::
/// min_depth_between` is USDC notional (the quoter compares Σ price·size).
/// Blank = 0 (no depth requirement).
fn parse_usd(s: &str) -> Option<Decimal> {
    let t = s.trim().trim_start_matches('$').replace(',', "");
    if t.is_empty() {
        return Some(dec!(0));
    }
    t.parse::<Decimal>().ok().filter(|v| *v >= dec!(0))
}

/// "3.0" (cents) → 0.03 (price units).
fn parse_cents(s: &str) -> Option<Decimal> {
    let cents: Decimal = s.trim().parse().ok()?;
    Some(cents / Decimal::from(100))
}

/// Optional auto-pause threshold in cents; blank = disabled.
fn parse_volatility(s: &str) -> Result<Option<Decimal>, String> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    match parse_cents(s) {
        Some(v) if v > dec!(0) => Ok(Some(v)),
        _ => Err("Auto-pause threshold must be a positive number of cents.".to_string()),
    }
}

/// POST /markets/start — turns the visual placement into 1 (one side) or 2
/// (both sides, sharing `condition_id`, size split in half) `MarketConfig`s.
pub async fn start_farming(
    State(state): State<WebState>,
    session: Session,
    Form(form): Form<StartFarmingForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return render_start_error("Invalid session — reload the page.");
    }
    let price = match form.price_cents.trim().parse::<Decimal>() {
        Ok(c) if c > dec!(0) && c < dec!(100) => c / dec!(100),
        _ => return render_start_error("Enter a price between 0 and 100¢."),
    };
    let order_size: Decimal = match form.order_size.trim().parse() {
        Ok(s) if s > dec!(0) => s,
        _ => return render_start_error("Order size must be a positive number."),
    };
    let Some(min_depth_between) = parse_usd(&form.min_depth_usd) else {
        return render_start_error("Min depth must be a dollar amount (e.g. 500).");
    };
    let max_volatility = match parse_volatility(&form.max_volatility_cents) {
        Ok(v) => v,
        Err(e) => return render_start_error(&e),
    };
    let expires_at = match parse_expiry(&form.expires_in) {
        Ok(e) => e,
        Err(e) => return render_start_error(&e),
    };

    let mr = match load_market(&form.slug).await {
        Ok(m) => m,
        Err(e) => return render_start_error(&e),
    };
    let side = form.side.min(mr.outcomes.len().saturating_sub(1));
    let both_sides = form.sides.as_deref() == Some("both");

    // Money path: always read a fresh REST book for the benchmark.
    let book_a = match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(&mr.token_ids[side])).await {
        Ok(Ok(b)) => b,
        _ => return render_start_error("Could not load the order book — try again."),
    };
    let Some(best_bid_a) = book_a.best_bid else {
        return render_start_error("No live bids on this market right now.");
    };
    if price >= best_bid_a {
        return render_start_error("Your price must be below the current best bid.");
    }
    // Both sides share this distance — applied to the other leg's own best bid.
    let distance = best_bid_a - price;
    let per_side_size = if both_sides { order_size / dec!(2) } else { order_size };

    let make = |token_idx: usize, benchmark: Decimal| MarketConfig {
        id: new_config_id(),
        url: poly_event_url(&mr.event_slug, &mr.market_slug),
        label: mr.question.clone(),
        condition_id: mr.condition_id.clone(),
        token_id: mr.token_ids[token_idx].clone(),
        token_label: mr.outcomes[token_idx].clone(),
        tick_size: mr.tick_size,
        distance,
        min_depth_between,
        order_size: per_side_size,
        expires_at,
        paused: false,
        benchmark_bid: Some(benchmark),
        max_volatility,
    };
    let mut new_configs = vec![make(side, best_bid_a)];
    if both_sides {
        let other = 1 - side;
        let book_b = match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(&mr.token_ids[other])).await {
            Ok(Ok(b)) => b,
            _ => return render_start_error("Could not load the order book for the other side — try again."),
        };
        let Some(best_bid_b) = book_b.best_bid else {
            return render_start_error("No live bids on the other side right now.");
        };
        new_configs.push(make(other, best_bid_b));
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
        // Engine not running yet: ws_manager derives its subscriptions from
        // AppState.configs on its first connect.
    }

    let mut resp = Html(String::new()).into_response();
    resp.headers_mut().insert("HX-Redirect", "/markets".parse().expect("static header value"));
    resp
}

fn new_config_id() -> String {
    format!("mar_{}_{}", Utc::now().timestamp(), rand_alpha(6))
}

/// The canonical Polymarket URL for a market. Grouped markets live at
/// `/event/{event_slug}/{market_slug}`; standalone binaries at
/// `/event/{market_slug}`. The market slug stays the final segment.
fn poly_event_url(event_slug: &str, market_slug: &str) -> String {
    if event_slug.is_empty() || event_slug == market_slug {
        format!("https://polymarket.com/event/{market_slug}")
    } else {
        format!("https://polymarket.com/event/{event_slug}/{market_slug}")
    }
}

/// Parses "<number><unit>" (s/m/h/d), e.g. "7d", "4h", "30m", or "never".
/// Range: 1 minute .. 1 year. Empty → 7 days.
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
    shell: Shell,
    q: String,
    sort: String,
    dir: String,
}

pub async fn browse_page(session: Session, Query(params): Query<BrowseParams>) -> Html<String> {
    render(&BrowsePageTemplate {
        shell: shell(&session, "markets").await,
        q: params.q.clone(),
        sort: params.sort_or_default(),
        dir: params.dir_or_default(),
    })
}

pub struct BrowseRow {
    pub question: String,
    pub market_slug: String,
    pub image: Option<String>,
    pub daily_pool: String,
    pub min_size: String,
    pub max_spread: String,
    pub spread_now: String,
    pub qualifies_now: bool,
    pub volume_24hr: String,
    pub ends: String,
    pub outcomes: Vec<String>,
    pub prices_cents: Vec<String>,
    /// One-sided earns 0 here (midpoint outside 10–90¢).
    pub needs_two_sided: bool,
    pub farming: bool,
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
    append: bool,
}

/// htmx target for the browse page's search/sort/pagination.
pub async fn browse_results(State(state): State<WebState>, Query(params): Query<BrowseParams>) -> Html<String> {
    let query = markets_browse::BrowseQuery {
        q: (!params.q.is_empty()).then(|| params.q.clone()),
        order_by: Some(params.sort_or_default()),
        position: Some(params.dir_or_default()),
        page_size: Some(BROWSE_PAGE_SIZE),
        next_cursor: params.cursor.clone(),
    };
    let tracked: Vec<String> = state.engine.read().await.configs.iter().map(|c| c.condition_id.to_lowercase()).collect();

    // Polymarket's browse endpoint takes several seconds; cache each query
    // briefly so paging back / revisiting is instant.
    let key = browse_key(&params.q, &params.sort_or_default(), &params.dir_or_default(), params.cursor.as_deref().unwrap_or(""));
    let outcome = match BROWSE.peek(&key) {
        // Cached (fresh or stale): answer now; a stale entry refreshes behind the scenes.
        Some((resp, _)) => {
            let q = query.clone();
            BROWSE.get_swr(&key, move || async move { markets_browse::browse(&q).await.map(Arc::new) });
            Ok(Ok(resp))
        }
        None => {
            tokio::time::timeout(
                Duration::from_secs(15),
                BROWSE.get_or_fetch(&key, || async { markets_browse::browse(&query).await.map(Arc::new) }),
            )
            .await
        }
    };
    let (rows, has_more, next_cursor, error) = match outcome {
        Ok(Ok(resp)) => {
            let resp = Arc::unwrap_or_clone(resp);
            let next_cursor = resp.next_cursor.unwrap_or_default();
            let has_more = !next_cursor.is_empty() && next_cursor != TERMINAL_CURSOR;
            (resp.data.into_iter().map(|m| to_browse_row(m, &tracked)).collect(), has_more, next_cursor, None)
        }
        Ok(Err(e)) => (Vec::new(), false, String::new(), Some(format!("Could not load markets: {e}"))),
        Err(_) => (Vec::new(), false, String::new(), Some("Request timed out — try again.".to_string())),
    };

    render(&BrowseResultsTemplate {
        rows,
        has_more,
        next_cursor,
        q: params.q.clone(),
        sort: params.sort_or_default(),
        dir: params.dir_or_default(),
        error,
        append: params.cursor.is_some(),
    })
}

/// The rewards API sends Postgres-style timestamps ("2029-01-01 04:59:00+00");
/// accept those and RFC 3339.
fn parse_end_date(s: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .or_else(|_| chrono::DateTime::parse_from_str(&format!("{s}00"), "%Y-%m-%d %H:%M:%S%z"))
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn to_browse_row(m: markets_browse::RewardsMultiMarket, tracked: &[String]) -> BrowseRow {
    let daily_pool: Decimal = m.rewards_config.iter().map(|c| c.rate_per_day).sum();
    let qualifies_now = m.spread <= m.rewards_max_spread;
    let first_price = m.tokens.first().map(|t| t.price).unwrap_or(dec!(0.5));
    let farming = tracked.contains(&m.condition_id.to_lowercase());
    let (outcomes, prices_cents) = m.tokens.into_iter().map(|t| (t.outcome, cents(t.price))).unzip();
    BrowseRow {
        question: m.question,
        market_slug: m.market_slug,
        image: m.image.filter(|s| !s.is_empty()),
        daily_pool: format!("${}", daily_pool.round_dp(0)),
        min_size: format!("{}", m.rewards_min_size.normalize()),
        max_spread: format!("±{}¢", m.rewards_max_spread.normalize()),
        spread_now: format!("{:.1}¢", m.spread * dec!(100)),
        qualifies_now,
        volume_24hr: m.volume_24hr.map(market_data::fmt_usd).unwrap_or_else(|| "—".to_string()),
        ends: m.end_date.as_deref().and_then(parse_end_date).map(until).unwrap_or_else(|| "—".to_string()),
        outcomes,
        prices_cents,
        needs_two_sided: !market_data::single_sided_allowed(first_price),
        farming,
    }
}

// ── Resolve a pasted URL → straight to the view, or a picker ────────────────

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
    shell: Shell,
    candidates: Vec<PickerCandidate>,
}

/// GET /markets/resolve?url= — paste-a-URL entry point. A market URL (or an
/// event with one tradeable market) goes straight to the view; an event with
/// several candidates shows a picker instead of guessing.
pub async fn resolve_url(session: Session, Query(p): Query<ResolveUrlParams>) -> Response {
    let url = p.url.trim();
    if url.is_empty() {
        return view_error(&session, "Paste a Polymarket market or event URL first.").await;
    }
    match tokio::time::timeout(NETWORK_TIMEOUT, gamma_resolve::resolve_url(url)).await {
        Ok(Ok(gamma_resolve::Resolved::Single(mr))) => Redirect::to(&format!("/markets/view?slug={}", mr.market_slug)).into_response(),
        Ok(Ok(gamma_resolve::Resolved::Multiple(refs))) => {
            let candidates = refs.into_iter().map(to_picker_candidate).collect();
            render(&MarketPickerTemplate { shell: shell(&session, "markets").await, candidates }).into_response()
        }
        Ok(Err(e)) => view_error(&session, &format!("Could not resolve this URL: {e}")).await,
        Err(_) => view_error(&session, "Request to Polymarket timed out — try again.").await,
    }
}

fn to_picker_candidate(mr: MarketRef) -> PickerCandidate {
    let has_rewards = mr.has_rewards();
    let prices_cents = mr.outcome_prices.iter().map(|p| cents(*p)).collect();
    PickerCandidate {
        market_slug: mr.market_slug,
        label: mr.group_item_title.unwrap_or(mr.question),
        image: mr.image.filter(|s| !s.is_empty()),
        outcomes: mr.outcomes,
        prices_cents,
        has_rewards,
    }
}

// ── Edit a running leg in place ──────────────────────────────────────────────

#[derive(Template)]
#[template(path = "_edit_drawer.html")]
struct EditTemplate {
    csrf_token: String,
    id: String,
    label: String,
    side_label: String,
    slug: String,
    order_size: String,
    distance_cents: String,
    min_depth_usd: String,
    max_volatility_cents: String,
    expires: String,
    live: bool,
    error: Option<String>,
}

async fn edit_template(state: &WebState, session: &Session, id: &str, error: Option<String>) -> Option<EditTemplate> {
    let s = state.engine.read().await;
    let c = s.configs.iter().find(|c| c.id == id)?;
    Some(EditTemplate {
        csrf_token: csrf_token(session).await,
        id: c.id.clone(),
        label: c.label.clone(),
        side_label: c.token_label.clone(),
        slug: super::dashboard::slug_of(&c.url),
        order_size: c.order_size.normalize().to_string(),
        distance_cents: (c.distance * dec!(100)).normalize().to_string(),
        min_depth_usd: c.min_depth_between.normalize().to_string(),
        max_volatility_cents: c.max_volatility.map(|v| (v * dec!(100)).normalize().to_string()).unwrap_or_default(),
        expires: until(c.expires_at),
        live: matches!(s.order_status.get(&c.id), Some(OrderStatus::Live { .. })),
        error,
    })
}

/// GET /markets/{id}/edit — the edit drawer for one leg.
pub async fn edit_form(State(state): State<WebState>, session: Session, Path(id): Path<String>) -> Html<String> {
    match edit_template(&state, &session, &id, None).await {
        Some(t) => render(&t),
        None => Html("<div class=\"drawer-body\"><div class=\"alert err\">This market leg no longer exists.</div></div>".to_string()),
    }
}

#[derive(Deserialize)]
pub struct EditForm {
    csrf: String,
    order_size: String,
    distance_cents: String,
    #[serde(default)]
    min_depth_usd: String,
    #[serde(default)]
    max_volatility_cents: String,
    #[serde(default)]
    expires_in: String,
}

/// POST /markets/{id}/edit — write-through update of one leg. Size/distance
/// changes on a live order cancel it via the same path as pause, then nudge
/// the (unchanged) quoter, which re-places with the new parameters.
pub async fn edit_submit(
    State(state): State<WebState>,
    session: Session,
    Path(id): Path<String>,
    Form(form): Form<EditForm>,
) -> Response {
    let fail = |msg: String| {
        let state = state.clone();
        let session = session.clone();
        let id = id.clone();
        async move {
            match edit_template(&state, &session, &id, Some(msg)).await {
                Some(t) => render(&t).into_response(),
                None => Html("This market leg no longer exists.").into_response(),
            }
        }
    };
    if !verify_csrf(&session, &form.csrf).await {
        return fail("Invalid session — reload the page.".into()).await;
    }
    let order_size = match form.order_size.trim().parse::<Decimal>() {
        Ok(v) if v > dec!(0) => v,
        _ => return fail("Order size must be a positive dollar amount.".into()).await,
    };
    let distance = match parse_cents(&form.distance_cents) {
        Some(v) if v > dec!(0) => v,
        _ => return fail("Distance must be a positive number of cents.".into()).await,
    };
    let Some(min_depth) = parse_usd(&form.min_depth_usd) else {
        return fail("Min depth must be a dollar amount (e.g. 500).".into()).await;
    };
    let max_volatility = match parse_volatility(&form.max_volatility_cents) {
        Ok(v) => v,
        Err(e) => return fail(e).await,
    };
    let expires_at = match form.expires_in.trim() {
        "" | "keep" => None,
        other => match parse_expiry(other) {
            Ok(t) => Some(t),
            Err(e) => return fail(e).await,
        },
    };

    // A new auto-pause threshold is measured from today's best bid.
    let (token, old_vol) = {
        let s = state.engine.read().await;
        match s.configs.iter().find(|c| c.id == id) {
            Some(c) => (c.token_id.clone(), c.max_volatility),
            None => return fail("This market leg no longer exists.".into()).await,
        }
    };
    let new_benchmark = if max_volatility.is_some() && max_volatility != old_vol {
        let engine_bb = state.engine.read().await.books.get(&token).and_then(|b| b.best_bid);
        match engine_bb {
            Some(bb) => Some(bb),
            None => match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(&token)).await {
                Ok(Ok(b)) if b.best_bid.is_some() => b.best_bid,
                _ => return fail("Could not read the current best bid for the auto-pause benchmark — try again.".into()).await,
            },
        }
    } else {
        None
    };

    let (old, requote_order, snapshot, markets_file) = {
        let mut s = state.engine.write().await;
        let live_order = s.configs.iter().find(|c| c.id == id).and_then(|c| match s.order_status.get(&c.id) {
            Some(OrderStatus::Live { order_id, .. }) => Some(order_id.clone()),
            _ => None,
        });
        let Some(cfg) = s.configs.iter_mut().find(|c| c.id == id) else {
            drop(s);
            return fail("This market leg no longer exists.".into()).await;
        };
        let old = cfg.clone();
        cfg.order_size = order_size;
        cfg.distance = distance;
        cfg.min_depth_between = min_depth;
        cfg.max_volatility = max_volatility;
        if let Some(bb) = new_benchmark {
            cfg.benchmark_bid = Some(bb);
        }
        if let Some(t) = expires_at {
            cfg.expires_at = t;
        }
        if let Err(e) = cfg.validate() {
            *cfg = old;
            drop(s);
            return fail(e).await;
        }
        let quote_changed = old.order_size != order_size || old.distance != distance;
        let requote = if quote_changed { live_order } else { None };
        (old, requote, s.configs.clone(), s.markets_file.clone())
    };

    if let Err(e) = save_markets(&markets_file, &snapshot) {
        let mut s = state.engine.write().await;
        if let Some(cfg) = s.configs.iter_mut().find(|c| c.id == id) {
            *cfg = old;
        }
        drop(s);
        return fail(format!("Failed to save markets.json: {e}")).await;
    }

    if let Some(order_id) = requote_order {
        cancel_leg_order(&state, &id, &order_id).await;
    }
    state.quote_nudge.notify_one();

    let mut resp = Html(String::new()).into_response();
    resp.headers_mut().insert("HX-Trigger", r#"{"pf-saved":"Changes saved"}"#.parse().expect("static header value"));
    resp
}

// ── Remove / pause / resume ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MarketActionForm {
    csrf: String,
}

/// Cancel one leg's live order and return it to Idle once confirmed off the
/// book. Unconfirmed cancels stay `Cancelling`; the engine's existing 30s
/// timeout recovery in `evaluate_all_markets` handles them.
async fn cancel_leg_order(state: &WebState, id: &str, order_id: &str) {
    {
        let mut s = state.engine.write().await;
        s.order_status.insert(
            id.to_string(),
            OrderStatus::Cancelling { order_id: order_id.to_string(), since: std::time::Instant::now() },
        );
    }
    if let Some((executor, _)) = state.engine_handle.get().await {
        let confirmed = executor.cancel_order_verified(order_id).await.unwrap_or(false);
        if confirmed {
            let mut s = state.engine.write().await;
            if matches!(s.order_status.get(id), Some(OrderStatus::Cancelling { order_id: oid, .. }) if oid == order_id) {
                s.order_status.insert(id.to_string(), OrderStatus::Idle);
            }
        }
    }
}

/// POST /markets/{id}/remove — removes one leg and cancels its resting order.
pub async fn remove_market(
    State(state): State<WebState>,
    session: Session,
    Path(id): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return render_markets_table(&state, &session, Some("Invalid session — reload the page.".into())).await.into_response();
    }

    let (configs_snapshot, removed, orders_to_cancel, tokens_to_unsubscribe, markets_file) = {
        let mut s = state.engine.write().await;
        let removed: Vec<_> = s.configs.iter().filter(|c| c.id == id).cloned().collect();
        if removed.is_empty() {
            drop(s);
            return render_markets_table(&state, &session, None).await.into_response();
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
        s.configs.retain(|c| c.id != id);
        let tokens_to_unsubscribe: Vec<String> = removed
            .iter()
            .map(|c| c.token_id.clone())
            .filter(|tid| !s.configs.iter().any(|c| &c.token_id == tid))
            .collect();
        (s.configs.clone(), removed, orders_to_cancel, tokens_to_unsubscribe, s.markets_file.clone())
    };

    if let Err(e) = save_markets(&markets_file, &configs_snapshot) {
        let mut s = state.engine.write().await;
        for c in &removed {
            s.order_status.insert(c.id.clone(), OrderStatus::Idle);
        }
        s.configs.extend(removed);
        drop(s);
        tracing::error!("Failed to save markets.json after remove: {}", e);
        return render_markets_table(&state, &session, Some(format!("Failed to save: {e}"))).await.into_response();
    }

    let mut flash = None;
    if let Some((executor, ws_cmd_tx)) = state.engine_handle.get().await {
        if !orders_to_cancel.is_empty() {
            if let Err(e) = executor.cancel_orders(&orders_to_cancel).await {
                flash = Some(format!("Removed, but cancelling its order failed — check Polymarket: {e}"));
            }
        }
        if !tokens_to_unsubscribe.is_empty() {
            let _ = ws_cmd_tx.send(WsCommand::Unsubscribe(tokens_to_unsubscribe)).await;
        }
    }
    render_markets_table(&state, &session, flash).await.into_response()
}

/// Pause/resume one leg (`pausing` picks the direction).
async fn set_paused(state: &WebState, id: &str, pausing: bool) -> Result<(), String> {
    let (configs_snapshot, order_to_cancel, markets_file) = {
        let mut s = state.engine.write().await;
        if !s.configs.iter().any(|c| c.id == id) {
            return Ok(()); // already gone
        }
        for c in s.configs.iter_mut().filter(|c| c.id == id) {
            c.paused = pausing;
        }
        let order_to_cancel = if pausing {
            match s.order_status.get(id) {
                Some(OrderStatus::Live { order_id, .. }) => Some(order_id.clone()),
                _ => None,
            }
        } else {
            None
        };
        (s.configs.clone(), order_to_cancel, s.markets_file.clone())
    };

    if let Err(e) = save_markets(&markets_file, &configs_snapshot) {
        let mut s = state.engine.write().await;
        for c in s.configs.iter_mut().filter(|c| c.id == id) {
            c.paused = !pausing;
        }
        return Err(format!("Failed to save: {e}"));
    }

    if let Some(order_id) = order_to_cancel {
        cancel_leg_order(state, id, &order_id).await;
    }
    // On resume, poke the quote loop so it re-quotes right away.
    if !pausing {
        state.quote_nudge.notify_one();
    }
    Ok(())
}

async fn pause_resume(state: WebState, session: Session, ids: Vec<String>, csrf: &str, pausing: bool) -> Response {
    if !verify_csrf(&session, csrf).await {
        return render_markets_table(&state, &session, Some("Invalid session — reload the page.".into())).await.into_response();
    }
    let mut flash = None;
    for id in &ids {
        if let Err(e) = set_paused(&state, id, pausing).await {
            flash = Some(e);
            break;
        }
    }
    render_markets_table(&state, &session, flash).await.into_response()
}

pub async fn pause_market(
    State(state): State<WebState>,
    session: Session,
    Path(id): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    pause_resume(state, session, vec![id], &form.csrf, true).await
}

pub async fn resume_market(
    State(state): State<WebState>,
    session: Session,
    Path(id): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    pause_resume(state, session, vec![id], &form.csrf, false).await
}

async fn group_ids(state: &WebState, cid: &str) -> Vec<String> {
    state.engine.read().await.configs.iter().filter(|c| c.condition_id == cid).map(|c| c.id.clone()).collect()
}

/// POST /markets/group/{cid}/pause — every leg of one market.
pub async fn pause_group(
    State(state): State<WebState>,
    session: Session,
    Path(cid): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    let ids = group_ids(&state, &cid).await;
    pause_resume(state, session, ids, &form.csrf, true).await
}

/// POST /markets/group/{cid}/resume
pub async fn resume_group(
    State(state): State<WebState>,
    session: Session,
    Path(cid): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    let ids = group_ids(&state, &cid).await;
    pause_resume(state, session, ids, &form.csrf, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_depth_is_parsed_as_usd_not_cents() {
        // The quoter compares min_depth_between to USDC notional — "500"
        // must mean $500 (the old form divided by 100 → $5).
        assert_eq!(parse_usd("500"), Some(dec!(500)));
        assert_eq!(parse_usd("$1,250.50"), Some(dec!(1250.50)));
        assert_eq!(parse_usd(""), Some(dec!(0)));
        assert_eq!(parse_usd("-5"), None);
        assert_eq!(parse_usd("abc"), None);
    }

    #[test]
    fn volatility_and_expiry_parsing() {
        assert_eq!(parse_volatility(""), Ok(None));
        assert_eq!(parse_volatility("5"), Ok(Some(dec!(0.05))));
        assert!(parse_volatility("0").is_err());
        assert!(parse_expiry("never").unwrap() > Utc::now() + chrono::Duration::days(3650));
        assert!(parse_expiry("2x").is_err());
        assert!(parse_expiry("400d").is_err());
    }

    #[test]
    fn end_dates_in_both_formats() {
        assert!(parse_end_date("2029-01-01 04:59:00+00").is_some());
        assert!(parse_end_date("2029-01-01T04:59:00Z").is_some());
        assert!(parse_end_date("soon").is_none());
    }

    #[test]
    fn event_urls() {
        assert_eq!(poly_event_url("ev", "mk"), "https://polymarket.com/event/ev/mk");
        assert_eq!(poly_event_url("mk", "mk"), "https://polymarket.com/event/mk");
    }
}
