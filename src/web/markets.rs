//! Market lifecycle: browse reward-eligible markets, resolve a pasted URL, the
//! interactive market view (live book, price chart, placement preview), and
//! add / edit / pause / resume / remove. Write-through — handlers mutate
//! `AppState` directly and persist atomically while still holding the engine
//! lock (rollback on save failure).

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
use rust_decimal::{Decimal, RoundingStrategy};
use rust_decimal_macros::dec;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::RwLock;
use tower_sessions::Session;

use crate::cache::TtlCache;
use crate::engine::executor::Executor;
use crate::engine::placer::OrderApi;
use crate::engine::ws_manager::AppState;
use crate::rewards::gamma_resolve::{self, MarketRef};
use crate::rewards::{market_data, market_flow, markets_browse};
use crate::types::{EnginePhase, MarketConfig, OrderStatus, WsCommand};

use super::auth::{csrf_token, verify_csrf};
use super::dashboard::{cents, render_markets_table, until};
use super::input;
use super::rewards;
use super::shell::{render, shell, Shell};
use super::state::WebState;

/// Hard upper bound on any handler-initiated network call — a request
/// handler must not hang on a flaky upstream.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(8);
/// Polymarket's pagination sentinel (base64 of "-1").
const TERMINAL_CURSOR: &str = "LTE=";
/// Book levels per side on the ladder (the ladder area scrolls).
const LADDER_LEVELS: usize = 50;
/// Live-book pushes are coalesced to at most one per this interval.
const BOOK_PUSH_MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Browse results (query → response). Polymarket's endpoint takes ~5–10s, so
/// results are served stale-while-revalidate: instant after the first load.
static BROWSE: LazyLock<Arc<TtlCache<Arc<markets_browse::RewardsMultiResponse>>>> =
    LazyLock::new(|| Arc::new(TtlCache::with_capacity(Duration::from_secs(60), 64)));
const BROWSE_PAGE_SIZE: u32 = 30;

/// Schemes a pasted market URL may have. Anything else (file:, javascript:,
/// data:, …) can't be a Polymarket link, so it is rejected up front. The URL
/// itself is never fetched; only the slug taken from it is sent to Gamma.
const ALLOWED_URL_SCHEMES: &[&str] = &["http", "https"];

/// Sanitize a user-entered market/event URL before resolving it.
///
/// Pasted URLs can contain invisible control characters (NUL, BEL, ESC, …)
/// that slip through copy/paste, OCR, or hand-editing and confuse the link
/// reader (`gamma_resolve::resolve_url`) into producing garbage. They are stripped here.
/// Share links (e.g. `.../event/some-market?tid=123` or `...#fragment`) also
/// carry query strings and fragments that would leak into the slug, so
/// everything from the first `?` or `#` onward is cut as well. The scheme is
/// then validated: only `http`/`https` are accepted, so a pasted `javascript:`
/// or `file:` URL is rejected before the slug is parsed (the pasted URL is
/// never fetched; only the extracted slug is sent to the Gamma API).
///
/// Returns the cleaned URL on success, or an error message suitable for the
/// user-facing error template.
fn sanitize_market_url(raw: &str) -> Result<String, String> {
    let cleaned: String = raw.chars().filter(|&c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Err("Paste a Polymarket market or event URL first.".into());
    }
    // A bare slug ("lol-blg-gen-2026-11-14") is read as an event page link.
    if gamma_resolve::valid_slug(trimmed) {
        return Ok(format!("https://polymarket.com/event/{trimmed}"));
    }

    // If the input starts with a URI scheme (RFC 3986 `scheme:`), the scheme
    // must be http or https. This is detected even without `//` — e.g.
    // `javascript:alert(1)` has a scheme and is rejected, not treated as
    // scheme-less. Host:port inputs (`polymarket.com:443`) are NOT schemes:
    // a dotted head is a hostname, so they fall through and get `https://`
    // prepended, same as plain scheme-less URLs (`polymarket.com/event/x`).
    let normalized = match split_scheme(trimmed) {
        Some((scheme, rest)) => {
            if !ALLOWED_URL_SCHEMES.iter().any(|s| *s == scheme) {
                return Err(format!("URL scheme must be http or https (got \"{scheme}\")."));
            }
            if rest.starts_with("//") {
                trimmed.to_string()
            } else {
                // `http:host/path` (no `//`) — normalise to `http://host/path`.
                format!("{scheme}://{rest}")
            }
        }
        None => format!("https://{}", trimmed),
    };

    // Drop query strings and fragments: share links append tracking params
    // (?tid=...) and anchors (#...) that would otherwise pollute the slug
    // the extractor derives from the path.
    let path_only = match normalized.find(['?', '#']) {
        Some(idx) => &normalized[..idx],
        None => &normalized,
    };
    if path_only.is_empty() {
        return Err("Paste a Polymarket market or event URL first.".into());
    }
    Ok(path_only.to_string())
}

/// Splits a leading URI scheme off `input` per RFC 3986
/// (`^[a-zA-Z][a-zA-Z0-9+.-]*:`), returning `(scheme_lowercased, rest)`.
/// A dotted head (e.g. `polymarket.com:443`) is a host:port, not a scheme,
/// so `None` is returned for it — callers treat that as scheme-less.
fn split_scheme(input: &str) -> Option<(String, &str)> {
    let idx = input.find(':')?;
    let head = &input[..idx];
    let mut chars = head.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !head.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
        return None;
    }
    if head.contains('.') {
        return None; // dotted head = hostname (host:port), not a scheme
    }
    Some((head.to_ascii_lowercase(), &input[idx + 1..]))
}

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
    if slug.is_empty() || slug.len() > input::MAX_SLUG_LEN {
        return Err("That doesn't look like a Polymarket market link.".to_string());
    }
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

/// One outcome's pre-rendered data. Both sides are embedded so switching the
/// outcome tab is a client-side swap; the live book re-streams for that side.
pub struct SideView {
    pub idx: usize,
    pub label: String,
    pub price_cents: String,
    /// Price (¢) prefilled when this outcome is selected — the "balanced"
    /// reward-weight vs fill-risk starting point, "" if no reward program.
    pub default_price_cents: String,
    pub selected: bool,
    /// This outcome's token id (to tell which outcomes you already farm).
    pub token: String,
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

/// Sensible prefilled bid: the Balanced preset (≈50% reward weight), else the
/// first available one.
fn default_price_cents(mid: Decimal, max_spread: Decimal, tick: Decimal, best_bid: Option<Decimal>) -> String {
    let presets = market_data::placement_presets(mid, max_spread, tick, best_bid);
    presets
        .iter()
        .find(|p| p.key == "balanced")
        .or_else(|| presets.first())
        .map(|p| (p.price * dec!(100)).normalize().to_string())
        .unwrap_or_default()
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
    /// What the market pays in liquidity rewards per day, e.g. "$1,905".
    reward_pool: String,
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
    /// The safeguards the form starts with (Settings → Defaults for new markets).
    d: super::defaults::FormDefaults,
    /// Default price (¢) prefilled: the Balanced preset, else the first one.
    default_price_cents: String,
    /// Tick in cents (input step/min) and the highest valid price in cents.
    tick_cents: String,
    price_max_cents: String,
    /// Reward minimum in SHARES ("" if no rewards) and max spread in cents
    /// ("0" if none) — feed the instant client-side score calc.
    reward_min_shares: String,
    reward_max_spread_cents: String,
    /// Trade-size filter buttons and the one the panel loads with.
    trade_filters: Vec<GroupOpt>,
    default_trade_filter: u32,
}

#[derive(Template)]
#[template(path = "market_view_error.html")]
struct MarketViewErrorTemplate {
    shell: Shell,
    message: String,
}

async fn view_error(session: &Session, message: &str) -> Response {
    render(&MarketViewErrorTemplate { shell: shell(session, "markets").await, message: message.to_string() })
        .into_response()
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
    let defaults = super::defaults::load(state.store.dir());
    let book = current_book(&state, &mr, side).await;
    let mid = midpoint_for(&mr, side, book.as_ref()).await;
    let (bb, ba) = book.as_ref().map(|b| (b.best_bid, b.best_ask)).unwrap_or((None, None));

    // Binary books mirror: the other outcome's bid = 1 − this ask, mid = 1 − mid.
    let side_data = |i: usize| -> (Decimal, Option<Decimal>) {
        if i == side {
            (mid, bb)
        } else {
            (dec!(1) - mid, ba.map(|a| dec!(1) - a))
        }
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
                default_price_cents: default_price_cents(m, max_spread, mr.tick_size, b),
                selected: i == side,
                token: mr.token_ids.get(i).cloned().unwrap_or_default(),
            }
        })
        .collect();
    let default_price_cents = sides[side].default_price_cents.clone();

    let ladder = match &book {
        Some(b) => market_data::build_ladder(
            b,
            mid,
            max_spread,
            LADDER_LEVELS,
            &live_mine(&state, &mr.token_ids[side]).await,
            mr.tick_size,
        ),
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
        reward_pool: pool_label(mr.reward_pool),
        reward_min: mr.rewards_min_size.map(|s| format!("{} shares", s.normalize())).unwrap_or_else(|| "—".to_string()),
        reward_max_spread: mr
            .rewards_max_spread
            .map(|s| format!("±{}¢", s.normalize()))
            .unwrap_or_else(|| "—".to_string()),
        volume_24hr: mr.volume_24hr.filter(|v| *v > dec!(0)).map(market_data::fmt_usd),
        liquidity: mr.liquidity.filter(|l| *l > dec!(0)).map(market_data::fmt_usd),
        resolves: mr.end_date.map(|d| d.format("%b %-d, %Y").to_string()),
        countdown: mr.end_date.map(|d| {
            let days = (d - now).num_days();
            if days > 1 {
                format!("{days}d left")
            } else if days == 1 {
                "1d left".into()
            } else if days == 0 {
                "ends today".into()
            } else {
                "ended".into()
            }
        }),
        farming,
        selected_label: mr.outcomes.get(side).cloned().unwrap_or_default(),
        group_opts: group_options(mr.tick_size),
        sides_both: p.sides.as_deref() == Some("both"),
        sides,
        selected_idx: side,
        ladder,
        default_size: defaults.size(),
        d: defaults,
        default_price_cents,
        tick_cents: (mr.tick_size * dec!(100)).normalize().to_string(),
        price_max_cents: ((dec!(1) - mr.tick_size) * dec!(100)).normalize().to_string(),
        reward_min_shares: mr.rewards_min_size.map(|s| s.normalize().to_string()).unwrap_or_default(),
        reward_max_spread_cents: mr
            .rewards_max_spread
            .map(|s| s.normalize().to_string())
            .unwrap_or_else(|| "0".to_string()),
        trade_filters: market_flow::TRADE_FILTERS
            .iter()
            .map(|&v| GroupOpt {
                value: v.to_string(),
                label: market_flow::filter_label(v),
                selected: v == market_flow::DEFAULT_TRADE_FILTER,
            })
            .collect(),
        default_trade_filter: market_flow::DEFAULT_TRADE_FILTER,
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

// ── Top holders and recent trades (public Data API, read-only) ──────────────

/// Holders move slowly; trades are fetched when asked for, cached only long
/// enough to absorb double clicks.
static HOLDERS: LazyLock<Arc<TtlCache<Arc<Vec<market_flow::OutcomeHolders>>>>> =
    LazyLock::new(|| Arc::new(TtlCache::new(Duration::from_secs(30))));
static TRADES: LazyLock<Arc<TtlCache<Arc<Vec<market_flow::Trade>>>>> =
    LazyLock::new(|| Arc::new(TtlCache::new(Duration::from_secs(5))));

#[derive(Deserialize)]
pub struct FlowParams {
    pub slug: String,
    /// Trade-size filter in dollars (one of `market_flow::TRADE_FILTERS`).
    pub min: Option<u32>,
}

pub struct HolderRow {
    pub rank: usize,
    pub name: String,
    /// Lower-case address, or empty (then the name isn't linked).
    pub wallet: String,
    pub shares: String,
    pub value: String,
}

pub struct HolderColumn {
    pub label: String,
    pub mid_cents: String,
    pub rows: Vec<HolderRow>,
}

#[derive(Template)]
#[template(path = "_market_holders.html")]
struct HoldersTemplate {
    error: Option<String>,
    columns: Vec<HolderColumn>,
}

pub struct TradeRow {
    pub ts_iso: String,
    pub time: String,
    pub buy: bool,
    pub outcome: String,
    pub shares: String,
    pub price: String,
    pub usd: String,
    pub name: String,
    pub wallet: String,
}

#[derive(Template)]
#[template(path = "_market_trades.html")]
struct TradesTemplate {
    error: Option<String>,
    filter: String,
    rows: Vec<TradeRow>,
}

/// The midpoint of each outcome: outcome 0 from its book, outcome 1 mirrors it.
async fn outcome_mids(state: &WebState, mr: &MarketRef) -> [Decimal; 2] {
    let book = current_book(state, mr, 0).await;
    let mid = midpoint_for(mr, 0, book.as_ref()).await.clamp(dec!(0), dec!(1));
    [mid, dec!(1) - mid]
}

fn holder_columns(mr: &MarketRef, mids: [Decimal; 2], data: &[market_flow::OutcomeHolders]) -> Vec<HolderColumn> {
    (0..2)
        .map(|i| HolderColumn {
            label: mr.outcomes.get(i).cloned().unwrap_or_default(),
            mid_cents: cents(mids[i].round_dp_with_strategy(3, rust_decimal::RoundingStrategy::MidpointAwayFromZero)),
            rows: data
                .iter()
                .find(|o| o.outcome_index == i)
                .map(|o| {
                    o.holders
                        .iter()
                        .enumerate()
                        .map(|(n, h)| HolderRow {
                            rank: n + 1,
                            name: h.name.clone(),
                            wallet: h.wallet.clone(),
                            shares: market_flow::fmt_shares(h.shares),
                            value: market_data::fmt_usd(h.shares * mids[i]),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// GET /markets/view/holders?slug= — the top holders of both outcomes, valued
/// at the current midpoint.
pub async fn view_holders(State(state): State<WebState>, Query(p): Query<FlowParams>) -> Html<String> {
    let fail = |e: &str| render(&HoldersTemplate { error: Some(e.to_string()), columns: Vec::new() });
    let mr = match load_market(&p.slug).await {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    let cid = mr.condition_id.clone();
    let data = HOLDERS
        .get_or_fetch(&cid, || async {
            tokio::time::timeout(NETWORK_TIMEOUT, market_flow::top_holders(&cid))
                .await
                .map_err(|_| eyre::eyre!("timed out"))?
                .map(Arc::new)
        })
        .await;
    match data {
        Ok(d) => {
            let mids = outcome_mids(&state, &mr).await;
            render(&HoldersTemplate { error: None, columns: holder_columns(&mr, mids, &d) })
        }
        Err(e) => {
            tracing::debug!("holders unavailable: {e:#}");
            fail("Couldn't load holders from Polymarket. Try again in a moment.")
        }
    }
}

fn trade_rows(mr: &MarketRef, trades: &[market_flow::Trade]) -> Vec<TradeRow> {
    trades
        .iter()
        .map(|t| {
            let at = chrono::DateTime::from_timestamp(t.timestamp, 0).unwrap_or_default();
            TradeRow {
                ts_iso: at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                time: at.format("%b %-d, %H:%M UTC").to_string(),
                buy: t.side == market_flow::Side::Buy,
                outcome: mr.outcomes.get(t.outcome_index).cloned().unwrap_or_default(),
                shares: market_flow::fmt_shares(t.shares),
                price: cents(t.price.round_dp_with_strategy(3, rust_decimal::RoundingStrategy::MidpointAwayFromZero)),
                usd: super::dashboard::usd(t.usd()),
                name: t.name.clone(),
                wallet: t.wallet.clone(),
            }
        })
        .collect()
}

/// GET /markets/view/trades?slug=&min= — the latest trades of at least `min`
/// dollars, either side. Only fetched when the panel loads or a filter is clicked.
pub async fn view_trades(Query(p): Query<FlowParams>) -> Html<String> {
    let min = market_flow::trade_filter(p.min);
    let filter = market_flow::filter_label(min);
    let fail =
        |e: &str| render(&TradesTemplate { error: Some(e.to_string()), filter: filter.clone(), rows: Vec::new() });
    let mr = match load_market(&p.slug).await {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    let cid = mr.condition_id.clone();
    let data = TRADES
        .get_or_fetch(&format!("{cid}:{min}"), || async {
            tokio::time::timeout(NETWORK_TIMEOUT, market_flow::recent_trades(&cid, min))
                .await
                .map_err(|_| eyre::eyre!("timed out"))?
                .map(Arc::new)
        })
        .await;
    match data {
        Ok(d) => render(&TradesTemplate { error: None, filter: filter.clone(), rows: trade_rows(&mr, &d) }),
        Err(e) => {
            tracing::debug!("trades unavailable: {e:#}");
            fail("Couldn't load trades from Polymarket. Try again in a moment.")
        }
    }
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
    input::book_group(raw, tick)
}

async fn render_ladder(
    state: &WebState,
    mr: &MarketRef,
    side: usize,
    group: Decimal,
    book: &market_data::BookSnapshot,
) -> String {
    let mid = midpoint_for(mr, side, Some(book)).await;
    let max_spread = mr.rewards_max_spread.unwrap_or(dec!(0));
    let mine = live_mine(state, &mr.token_ids[side]).await;
    let ladder = market_data::build_ladder(book, mid, max_spread, LADDER_LEVELS, &mine, group);
    BookLadderTemplate { ladder }.render().unwrap_or_else(|e| super::shell::render_failed(&e))
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
    session: Session,
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
    let ctx = Ctx { state: state.clone(), mr, side, group, rx, _guard: guard, dirty: true, last_sent: None };

    let stream = futures_util::stream::unfold(ctx, |mut ctx| async move {
        let mr = ctx.mr.clone()?; // unknown market → end the stream
        let token = mr.token_ids[ctx.side].clone();
        loop {
            if ctx.dirty {
                let wait =
                    ctx.last_sent.map(|t| BOOK_PUSH_MIN_INTERVAL.saturating_sub(t.elapsed())).unwrap_or_default();
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
    Sse::new(super::events::while_signed_in(state, &session, stream)).keep_alive(KeepAlive::default())
}

// ── Your position (authenticated; only when you're farming this market) ─────

#[derive(Deserialize)]
pub struct PositionParams {
    pub cid: String,
}

/// The market page's "You're farming this" panel: the same legs, statuses and
/// actions as this market's row in the Markets table, so the two always agree.
#[derive(Template)]
#[template(path = "_your_position.html")]
struct YourPositionTemplate {
    /// The actions' form values ({"csrf", "view"}) as JSON, built with a JSON
    /// encoder so a stored condition id can never break out of it.
    vals: String,
    /// `None`: you don't farm this market (nothing is shown).
    row: Option<super::dashboard::MarketRow>,
    /// Token ids with a leg here, for the order form's "already farming" note.
    farmed_tokens: String,
    /// Polymarket's verdict over the live orders: Some(true) if any scores,
    /// Some(false) if none does, None when unknown (no live order / no engine).
    scoring: Option<bool>,
    reward_pct: Option<String>,
    earnings_today: Option<String>,
}

/// Render the panel for one market, with an error toast when `flash` is set.
/// Never waits on Polymarket: scoring and today's rewards come from caches
/// refreshed in the background, so the 5s refresh stays cheap.
pub(super) async fn render_position(state: &WebState, session: &Session, cid: &str, flash: Option<String>) -> Response {
    let row = super::dashboard::market_row(state, cid).await;
    let (farmed_tokens, scoring) = match &row {
        Some(r) => {
            let mut tokens: Vec<&str> = r.legs.iter().map(|l| l.token_id()).collect();
            tokens.sort_unstable();
            tokens.dedup();
            let verdicts: Vec<bool> = r.legs.iter().filter_map(|l| l.scoring).collect();
            let scoring = if verdicts.is_empty() { None } else { Some(verdicts.contains(&true)) };
            (tokens.join(","), scoring)
        }
        None => (String::new(), None),
    };
    let (reward_pct, earnings_today) = match (&row, rewards::today_entries_cached(state).await) {
        (Some(_), Some(entries)) => entries
            .iter()
            .find(|e| e.condition_id.to_string().eq_ignore_ascii_case(cid))
            .map(|e| (Some(format!("{:.2}%", e.earning_percentage)), Some(format!("${:.2}", rewards::earned(e)))))
            .unwrap_or((None, None)),
        _ => (None, None),
    };
    let view = row.as_ref().map(|r| r.condition_id.as_str()).unwrap_or_default();
    let vals = serde_json::json!({ "csrf": csrf_token(session).await, "view": view }).to_string();
    let tpl = YourPositionTemplate { vals, row, farmed_tokens, scoring, reward_pct, earnings_today };
    super::dashboard::with_error_toast(render(&tpl).into_response(), flash.as_deref())
}

/// GET /markets/view/position?cid= — the panel, loaded after the page and
/// refreshed every 5s and after every change.
pub async fn view_position(
    State(state): State<WebState>,
    session: Session,
    Query(p): Query<PositionParams>,
) -> Response {
    render_position(&state, &session, &p.cid, None).await
}

// ── Live placement preview ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PlacementPreviewForm {
    #[serde(default)]
    csrf: String,
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
    /// Its best bid is too low to bid the same distance below it.
    no_room: bool,
}

/// Where the other leg of a both-sides farm would bid: the same distance below
/// its own best bid, on the tick. `None` when that isn't a real price.
fn other_side_price(best_bid: Decimal, distance: Decimal, tick: Decimal) -> Option<Decimal> {
    let p = crate::engine::orderbook::TokenBook::snap_to_tick(best_bid - distance, tick);
    (p > dec!(0)).then_some(p)
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
pub async fn view_preview(
    State(state): State<WebState>,
    session: Session,
    Form(form): Form<PlacementPreviewForm>,
) -> Html<String> {
    if !verify_csrf(&session, &form.csrf).await {
        return render(&PlacementPreviewTemplate::message(true, Some("Session expired — reload the page.".into())));
    }
    let price = match input::price_cents(&form.price_cents) {
        Ok(p) => p,
        Err(e) => return render(&PlacementPreviewTemplate::message(true, Some(e))),
    };
    let order_size = match input::order_size_usd(&form.order_size) {
        Ok(s) => s,
        Err(e) => return render(&PlacementPreviewTemplate::message(true, Some(e))),
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
        return render(&PlacementPreviewTemplate::message(
            true,
            Some("Order book unavailable right now — try again.".into()),
        ));
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
                // Same rounding as the engine (2 decimals) and this leg's preview.
                let shares = if op > dec!(0) { (eval.per_side_size / op).round_dp(2) } else { dec!(0) };
                let meets = shares >= min_size;
                let w =
                    if op > dec!(0) && meets { market_data::score_weight(other_mid, op, max_spread) } else { dec!(0) };
                Some((
                    OtherLeg {
                        label: mr.outcomes.get(1 - side).cloned().unwrap_or_default(),
                        price_cents: cents(op),
                        weight_pct: pct(w),
                        meets_min: meets || op <= dec!(0),
                        shares: shares.normalize().to_string(),
                        no_room: op <= dec!(0),
                    },
                    w,
                ))
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
        per_side_size: super::dashboard::usd(eval.per_side_size),
        // As the engine places it: 2 decimals (`{:.0}` truncated, and hid a
        // shortfall like 49.6 shares under a 50 minimum).
        per_side_shares: eval.per_side_shares.normalize().to_string(),
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
    /// With `expires_in=custom`: a whole number of hours or days.
    #[serde(default)]
    expires_custom: String,
    #[serde(default)]
    expires_custom_unit: String,
    #[serde(default)]
    max_volatility_cents: String,
}

#[derive(Template)]
#[template(path = "_start_farming_result.html")]
struct StartFarmingResultTemplate {
    error: Option<String>,
}

/// How far below the best bid a Start at `price` rests, or why it can't start.
/// Held to Edit's distance limit, so a started leg can always be saved in Edit.
fn start_distance(best_bid: Decimal, price: Decimal) -> Result<Decimal, String> {
    if price >= best_bid {
        return Err("Your price must be below the current best bid.".into());
    }
    input::distance(best_bid - price)
}

/// The settings every leg of one Start shares.
struct StartSettings {
    distance: Decimal,
    min_depth_between: Decimal,
    /// Per side: a both-sides Start splits the order size in half.
    order_size: Decimal,
    expires_at: chrono::DateTime<Utc>,
    max_volatility: Option<Decimal>,
}

const NO_LIVE_BIDS: &str = "No live bids on this market right now.";

/// The legs a Start creates from fresh books, or why it can't: `book` is the
/// chosen side's, `other_book` the other side's when farming both. Each leg's
/// best bid becomes its auto-pause benchmark.
fn start_configs(
    mr: &MarketRef,
    side: usize,
    book: &market_data::BookSnapshot,
    other_book: Option<&market_data::BookSnapshot>,
    settings: &StartSettings,
) -> Result<Vec<MarketConfig>, String> {
    let make = |token_idx: usize, benchmark: Decimal| MarketConfig {
        id: new_config_id(),
        url: poly_event_url(&mr.event_slug, &mr.market_slug),
        label: mr.question.clone(),
        condition_id: mr.condition_id.clone(),
        token_id: mr.token_ids[token_idx].clone(),
        token_label: mr.outcomes[token_idx].clone(),
        tick_size: mr.tick_size,
        distance: settings.distance,
        min_depth_between: settings.min_depth_between,
        order_size: settings.order_size,
        expires_at: settings.expires_at,
        paused: false,
        benchmark_bid: Some(benchmark),
        max_volatility: settings.max_volatility,
    };
    let best_bid = book.best_bid.ok_or(NO_LIVE_BIDS)?;
    let mut configs = vec![make(side, best_bid)];
    if let Some(other_book) = other_book {
        let other = 1 - side;
        let best_bid_b = other_book.best_bid.ok_or("No live bids on the other side right now.")?;
        // The other leg bids the same distance below its own best bid: that must
        // be a real price, or the leg would sit waiting forever.
        if other_side_price(best_bid_b, settings.distance, mr.tick_size).is_none() {
            let other_label = mr.outcomes.get(other).cloned().unwrap_or_default();
            return Err(format!(
                "{other_label}'s best bid is {}¢, so there's no room {}¢ below it. Farm one side, or bid closer to \
                 the best bid.",
                cents(best_bid_b),
                cents(settings.distance)
            ));
        }
        configs.push(make(other, best_bid_b));
    }
    for cfg in &configs {
        cfg.validate()?;
    }
    // Polymarket rejects an order under the book's minimum size. When several
    // legs fall short, name the one needing the most, so its fix covers both.
    let books = std::iter::once(book).chain(other_book);
    let shortfall = configs
        .iter()
        .zip(books)
        .filter_map(|(cfg, book)| min_order_shortfall(cfg, book.best_bid?, book.min_order_size))
        .max_by_key(|(needed_usd, _)| *needed_usd);
    // The form's size is the total, split across both sides.
    if let Some((needed_usd, reason)) = shortfall {
        return Err(match other_book {
            Some(_) => format!("{reason} Enter at least ${:.2} (split across both sides).", needed_usd * dec!(2)),
            None => format!("{reason} Enter at least ${needed_usd:.2}."),
        });
    }
    Ok(configs)
}

/// The per-side size `cfg` needs to meet the market's minimum order of
/// `min_shares`, and why its own is too small, when it is (the caller says what
/// to enter). Shares are counted as the engine sizes the order, at the price
/// the leg rests at below `best_bid`. A minimum of 0 means none is known (only
/// books built from the live feed lack one; the minimum comes from the REST book).
fn min_order_shortfall(cfg: &MarketConfig, best_bid: Decimal, min_shares: Decimal) -> Option<(Decimal, String)> {
    if min_shares <= dec!(0) {
        return None;
    }
    let rest = other_side_price(best_bid, cfg.distance, cfg.tick_size)?;
    // `rest` is above 0, the only price `shares_from_usd` refuses.
    let shares = Executor::shares_from_usd(cfg.order_size, rest).ok()?;
    if shares >= min_shares {
        return None;
    }
    let needed_usd = (min_shares * rest).round_dp_with_strategy(2, RoundingStrategy::AwayFromZero);
    Some((
        needed_usd,
        format!(
            "Too small for this market: ${:.2} per side is {} shares of {} at {}¢; Polymarket's minimum order is {} \
             shares.",
            cfg.order_size,
            shares.round_dp_with_strategy(1, RoundingStrategy::ToZero),
            cfg.token_label,
            (rest * dec!(100)).normalize(),
            min_shares.normalize()
        ),
    ))
}

fn render_start_error(message: &str) -> Response {
    render(&StartFarmingResultTemplate { error: Some(message.to_string()) }).into_response()
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
    let price = match input::price_cents(&form.price_cents) {
        Ok(p) => p,
        Err(e) => return render_start_error(&e),
    };
    let order_size = match input::order_size_usd(&form.order_size) {
        Ok(s) => s,
        Err(e) => return render_start_error(&e),
    };
    let min_depth_between = match input::depth_usd(&form.min_depth_usd) {
        Ok(d) => d,
        Err(e) => return render_start_error(&e),
    };
    let max_volatility = match input::volatility_cents(&form.max_volatility_cents) {
        Ok(v) => v,
        Err(e) => return render_start_error(&e),
    };
    let expires_at = match input::expiry_choice(&form.expires_in, &form.expires_custom, &form.expires_custom_unit) {
        Ok(e) => e,
        Err(e) => return render_start_error(&e),
    };

    let mr = match load_market(&form.slug).await {
        Ok(m) => m,
        Err(e) => return render_start_error(&e),
    };
    // The best bid is on the tick, so an on-tick price gives an on-tick distance.
    if let Err(e) = input::on_tick(price, mr.tick_size, "Your price") {
        return render_start_error(&e);
    }
    let side = form.side.min(mr.outcomes.len().saturating_sub(1));
    let both_sides = form.sides.as_deref() == Some("both");

    // Money path: always read a fresh REST book for the benchmark.
    let book_a = match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(&mr.token_ids[side])).await {
        Ok(Ok(b)) => b,
        _ => return render_start_error("Could not load the order book — try again."),
    };
    state.caches.min_order_size.put(&mr.token_ids[side], book_a.min_order_size);
    let Some(best_bid_a) = book_a.best_bid else {
        return render_start_error(NO_LIVE_BIDS);
    };
    // Both sides share this distance — applied to the other leg's own best bid.
    let distance = match start_distance(best_bid_a, price) {
        Ok(d) => d,
        Err(e) => return render_start_error(&e),
    };
    let per_side_size = if both_sides { order_size / dec!(2) } else { order_size };
    let book_b = if both_sides {
        match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(&mr.token_ids[1 - side])).await {
            Ok(Ok(b)) => {
                state.caches.min_order_size.put(&mr.token_ids[1 - side], b.min_order_size);
                Some(b)
            }
            _ => return render_start_error("Could not load the order book for the other side — try again."),
        }
    } else {
        None
    };
    let settings = StartSettings { distance, min_depth_between, order_size: per_side_size, expires_at, max_volatility };
    let new_configs = match start_configs(&mr, side, &book_a, book_b.as_ref(), &settings) {
        Ok(c) => c,
        Err(e) => return render_start_error(&e),
    };

    let new_token_ids: Vec<String> = new_configs.iter().map(|c| c.token_id.clone()).collect();
    let newly_subscribed = {
        let mut s = state.engine.write().await;
        let newly_subscribed: Vec<String> =
            new_token_ids.iter().filter(|t| !s.configs.iter().any(|c| &c.token_id == *t)).cloned().collect();
        for cfg in &new_configs {
            s.order_status.insert(cfg.id.clone(), OrderStatus::Idle);
        }
        s.configs.extend(new_configs.iter().cloned());
        if let Err(e) = save_configs(&mut s) {
            // Roll back so disk and memory never diverge.
            let new_ids: Vec<&str> = new_configs.iter().map(|c| c.id.as_str()).collect();
            s.configs.retain(|c| !new_ids.contains(&c.id.as_str()));
            for cfg in &new_configs {
                s.order_status.remove(&cfg.id);
            }
            return render_start_error(&format!("Failed to save markets.json: {e}"));
        }
        newly_subscribed
    };

    if !newly_subscribed.is_empty() {
        if let Some((_, ws_cmd_tx)) = state.engine_handle.get().await {
            let _ = ws_cmd_tx.send(WsCommand::Subscribe(newly_subscribed)).await;
        }
        // Engine not running yet: ws_manager derives its subscriptions from
        // AppState.configs on its first connect.
    }

    let mut resp = Html(String::new()).into_response();
    resp.headers_mut().insert("HX-Redirect", axum::http::HeaderValue::from_static("/markets"));
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

fn rand_alpha(n: usize) -> String {
    use rand::RngExt;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..n).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
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

/// Sort columns the browse form offers. Anything else (the value goes upstream and
/// into a cache key) falls back to the default.
const BROWSE_SORTS: [&str; 5] = ["rate_per_day", "competitiveness", "spread", "volume_24hr", "end_date"];

impl BrowseParams {
    fn sort_or_default(&self) -> String {
        match self.sort.as_deref() {
            Some(s) if BROWSE_SORTS.contains(&s) => s.to_string(),
            _ => "rate_per_day".to_string(),
        }
    }
    fn dir_or_default(&self) -> String {
        match self.dir.as_deref() {
            Some("ASC") => "ASC".to_string(),
            _ => "DESC".to_string(),
        }
    }
    /// Search text, bounded.
    fn q(&self) -> String {
        input::search_text(&self.q)
    }
    /// Pagination cursor, if plausible.
    fn cursor(&self) -> Option<String> {
        input::cursor(self.cursor.as_deref())
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
        q: params.q(),
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
        q: Some(params.q()).filter(|q| !q.is_empty()),
        order_by: Some(params.sort_or_default()),
        position: Some(params.dir_or_default()),
        page_size: Some(BROWSE_PAGE_SIZE),
        next_cursor: params.cursor(),
    };
    let tracked: Vec<String> =
        state.engine.read().await.configs.iter().map(|c| c.condition_id.to_lowercase()).collect();

    // Polymarket's browse endpoint takes several seconds; cache each query
    // briefly so paging back / revisiting is instant.
    let key = browse_key(
        &params.q(),
        &params.sort_or_default(),
        &params.dir_or_default(),
        params.cursor().as_deref().unwrap_or(""),
    );
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
        q: params.q(),
        sort: params.sort_or_default(),
        dir: params.dir_or_default(),
        error,
        append: params.cursor().is_some(),
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

/// Time left on a listed market; one past its end date (still listed while
/// its rewards run) reads "ended" rather than "expired", which on this app
/// means one of your legs stopped quoting.
fn browse_ends(t: chrono::DateTime<Utc>) -> String {
    match until(t) {
        s if s == "expired" => "ended".to_string(),
        s => s,
    }
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
        daily_pool: super::dashboard::daily_rewards(daily_pool),
        min_size: format!("{}", m.rewards_min_size.normalize()),
        max_spread: format!("±{}¢", m.rewards_max_spread.normalize()),
        spread_now: format!("{:.1}¢", m.spread * dec!(100)),
        qualifies_now,
        volume_24hr: m.volume_24hr.map(market_data::fmt_usd).unwrap_or_else(|| "—".to_string()),
        ends: m.end_date.as_deref().and_then(parse_end_date).map(browse_ends).unwrap_or_else(|| "—".to_string()),
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
    pub reward_pool: String,
}

#[derive(Template)]
#[template(path = "market_picker.html")]
struct MarketPickerTemplate {
    shell: Shell,
    candidates: Vec<PickerCandidate>,
    /// How many candidates pay rewards (they are listed first).
    rewarded: usize,
}

/// GET /markets/resolve?url= — paste-a-URL entry point. A market URL (or an
/// event with one tradeable market) goes straight to the view; an event with
/// several candidates shows a picker instead of guessing.
pub async fn resolve_url(session: Session, Query(p): Query<ResolveUrlParams>) -> Response {
    let url = match sanitize_market_url(&p.url) {
        Ok(u) => u,
        Err(msg) => return view_error(&session, &msg).await,
    };
    match tokio::time::timeout(NETWORK_TIMEOUT, gamma_resolve::resolve_url(&url)).await {
        Ok(Ok(gamma_resolve::Resolved::Single(mr))) => {
            Redirect::to(&format!("/markets/view?slug={}", mr.market_slug)).into_response()
        }
        Ok(Ok(gamma_resolve::Resolved::Multiple(refs))) => {
            // A short group title ("Duration O/U 32:30") can repeat across a
            // game's markets; those rows show the full question instead.
            let mut titles: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for t in refs.iter().filter_map(|m| m.group_item_title.as_deref()).filter(|t| !t.is_empty()) {
                *titles.entry(t.to_string()).or_default() += 1;
            }
            let candidates: Vec<PickerCandidate> = refs
                .into_iter()
                .map(|mr| {
                    let unique = mr.group_item_title.as_deref().is_some_and(|t| titles.get(t) == Some(&1));
                    to_picker_candidate(mr, unique)
                })
                .collect();
            let rewarded = candidates.iter().filter(|c| c.has_rewards).count();
            render(&MarketPickerTemplate { shell: shell(&session, "markets").await, candidates, rewarded })
                .into_response()
        }
        Ok(Err(e)) => view_error(&session, &format!("Could not resolve this URL: {e}")).await,
        Err(_) => view_error(&session, "Request to Polymarket timed out — try again.").await,
    }
}

fn to_picker_candidate(mr: MarketRef, use_group_title: bool) -> PickerCandidate {
    let has_rewards = mr.has_rewards();
    let prices_cents = mr.outcome_prices.iter().map(|p| cents(*p)).collect();
    let label = match mr.group_item_title {
        Some(t) if use_group_title => t,
        _ => mr.question,
    };
    PickerCandidate {
        market_slug: mr.market_slug,
        label,
        image: mr.image.filter(|s| !s.is_empty()),
        outcomes: mr.outcomes,
        prices_cents,
        has_rewards,
        reward_pool: pool_label(mr.reward_pool),
    }
}

/// A market's daily rewards for display: "$1,905", "<$1", or "" for none.
fn pool_label(pool: Decimal) -> String {
    super::dashboard::daily_rewards(pool)
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
    /// The market's tick in cents: the distance input steps by it.
    tick_cents: String,
    min_depth_usd: String,
    max_volatility_cents: String,
    expires: String,
    /// Selected "Expires" option and the custom amount/unit shown with it.
    expires_choice: String,
    expires_custom: String,
    expires_custom_unit: String,
    live: bool,
    error: Option<String>,
    /// Opened from an expired leg's Extend button: focus starts on Expires.
    focus_expires: bool,
}

/// Longest typed value echoed back into the drawer after a rejected save.
const MAX_ECHO_LEN: usize = 64;

/// What the user typed, shown again when a save is rejected so nothing has
/// to be re-entered. Values are only echoed (escaped by the template), never
/// parsed here; the select options are limited to the known ones.
struct TypedEdit {
    order_size: String,
    distance_cents: String,
    min_depth_usd: String,
    max_volatility_cents: String,
    expires_choice: String,
    expires_custom: String,
    expires_custom_unit: String,
}

impl TypedEdit {
    fn from_form(f: &EditForm) -> Self {
        let echo = |v: &str| v.trim().chars().take(MAX_ECHO_LEN).collect::<String>();
        let choice = f.expires_in.trim();
        Self {
            order_size: echo(&f.order_size),
            distance_cents: echo(&f.distance_cents),
            min_depth_usd: echo(&f.min_depth_usd),
            max_volatility_cents: echo(&f.max_volatility_cents),
            expires_choice: if EXPIRY_CHOICES.contains(&choice) { choice.to_string() } else { "keep".to_string() },
            expires_custom: echo(&f.expires_custom),
            expires_custom_unit: if f.expires_custom_unit.trim() == "d" { "d" } else { "h" }.to_string(),
        }
    }
}

/// The edit drawer's "Expires" options.
const EXPIRY_CHOICES: [&str; 7] = ["keep", "1d", "3d", "7d", "30d", "never", "custom"];

async fn edit_template(
    state: &WebState,
    session: &Session,
    id: &str,
    error: Option<String>,
    typed: Option<&TypedEdit>,
) -> Option<EditTemplate> {
    let s = state.engine.read().await;
    let c = s.configs.iter().find(|c| c.id == id)?;
    let saved = TypedEdit {
        order_size: c.order_size.normalize().to_string(),
        distance_cents: (c.distance * dec!(100)).normalize().to_string(),
        min_depth_usd: c.min_depth_between.normalize().to_string(),
        max_volatility_cents: c.max_volatility.map(|v| (v * dec!(100)).normalize().to_string()).unwrap_or_default(),
        expires_choice: "keep".to_string(),
        expires_custom: String::new(),
        expires_custom_unit: "h".to_string(),
    };
    let v = typed.unwrap_or(&saved);
    Some(EditTemplate {
        csrf_token: csrf_token(session).await,
        id: c.id.clone(),
        label: c.label.clone(),
        side_label: c.token_label.clone(),
        slug: super::dashboard::slug_of(&c.url),
        order_size: v.order_size.clone(),
        distance_cents: v.distance_cents.clone(),
        tick_cents: (c.tick_size * dec!(100)).normalize().to_string(),
        min_depth_usd: v.min_depth_usd.clone(),
        max_volatility_cents: v.max_volatility_cents.clone(),
        expires: until(c.expires_at),
        expires_choice: v.expires_choice.clone(),
        expires_custom: v.expires_custom.clone(),
        expires_custom_unit: v.expires_custom_unit.clone(),
        live: matches!(s.order_status.get(&c.id), Some(OrderStatus::Live { .. })),
        error,
        focus_expires: false,
    })
}

#[derive(Deserialize)]
pub struct EditParams {
    /// `expires`: start on the Expires field (an expired leg's Extend button).
    #[serde(default)]
    focus: String,
}

/// GET /markets/{id}/edit — the edit drawer for one leg.
pub async fn edit_form(
    State(state): State<WebState>,
    session: Session,
    Path(id): Path<String>,
    Query(p): Query<EditParams>,
) -> Html<String> {
    match edit_template(&state, &session, &id, None, None).await {
        Some(t) => render(&EditTemplate { focus_expires: p.focus == "expires", ..t }),
        None => Html(
            "<div class=\"drawer-body\"><div class=\"alert err\">This market leg no longer exists.</div></div>"
                .to_string(),
        ),
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
    #[serde(default)]
    expires_custom: String,
    #[serde(default)]
    expires_custom_unit: String,
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
    let typed = TypedEdit::from_form(&form);
    let fail = |msg: String| {
        let state = state.clone();
        let session = session.clone();
        let id = id.clone();
        let typed = &typed;
        async move {
            match edit_template(&state, &session, &id, Some(msg), Some(typed)).await {
                Some(t) => render(&t).into_response(),
                None => Html("This market leg no longer exists.").into_response(),
            }
        }
    };
    if !verify_csrf(&session, &form.csrf).await {
        return fail("Invalid session — reload the page.".into()).await;
    }
    let order_size = match input::order_size_usd(&form.order_size) {
        Ok(v) => v,
        Err(e) => return fail(e).await,
    };
    let distance = match input::distance_cents(&form.distance_cents) {
        Ok(v) => v,
        Err(e) => return fail(e).await,
    };
    let min_depth = match input::depth_usd(&form.min_depth_usd) {
        Ok(v) => v,
        Err(e) => return fail(e).await,
    };
    let max_volatility = match input::volatility_cents(&form.max_volatility_cents) {
        Ok(v) => v,
        Err(e) => return fail(e).await,
    };
    let expires_at = match form.expires_in.trim() {
        "" | "keep" => None,
        other => match input::expiry_choice(other, &form.expires_custom, &form.expires_custom_unit) {
            Ok(t) => Some(t),
            Err(e) => return fail(e).await,
        },
    };

    let leg = {
        let s = state.engine.read().await;
        match s.configs.iter().find(|c| c.id == id) {
            Some(c) => match c.describe_problem() {
                // No token to look up a best bid for, and Edit can't set one.
                Some(problem) if c.token_id.is_empty() => {
                    drop(s);
                    return fail(problem).await;
                }
                _ => c.clone(),
            },
            None => return fail("This market leg no longer exists.".into()).await,
        }
    };
    let distance = match input::on_tick(distance, leg.tick_size, "Distance") {
        Ok(v) => v,
        Err(e) => return fail(e).await,
    };
    // Only size and distance change the order's shares.
    if leg.order_size != order_size || leg.distance != distance {
        if let Err(e) = check_min_order(&state, &MarketConfig { order_size, distance, ..leg.clone() }).await {
            return fail(e).await;
        }
    }
    // A new auto-pause threshold is measured from today's best bid.
    let new_benchmark = if max_volatility.is_some() && max_volatility != leg.max_volatility {
        match best_bid_now(&state, &leg.token_id).await {
            Some(bb) => Some(bb),
            None => {
                return fail("Could not read the current best bid for the auto-pause benchmark — try again.".into())
                    .await
            }
        }
    } else {
        None
    };

    let requote_order = {
        let mut s = state.engine.write().await;
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
        // Size or distance changed: settle whatever the leg holds once saved — a
        // live order, or one still being placed with the old settings.
        let requote = quote_changed.then_some(());
        if let Err(e) = save_configs(&mut s) {
            if let Some(cfg) = s.configs.iter_mut().find(|c| c.id == id) {
                *cfg = old;
            }
            drop(s);
            return fail(format!("Failed to save markets.json: {e}")).await;
        }
        requote
    };

    if requote_order.is_some() {
        cancel_leg_order(&state, &id).await;
    }
    state.quote_nudge.notify_one();

    let mut resp = Html(String::new()).into_response();
    resp.headers_mut().insert("HX-Trigger", axum::http::HeaderValue::from_static(r#"{"pf-saved":"Changes saved"}"#));
    resp
}

// ── Remove / pause / resume ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MarketActionForm {
    csrf: String,
    /// Set by the market page's panel (its market's condition id): reply with
    /// that panel instead of the Markets table. Only picks what to render.
    #[serde(default)]
    view: String,
}

/// The fragment a leg action answers with: the panel it was clicked in.
async fn reply(state: &WebState, session: &Session, view: &str, flash: Option<String>) -> Response {
    if view.is_empty() {
        render_markets_table(state, session, flash).await
    } else {
        render_position(state, session, view, flash).await
    }
}

/// Cancel whatever order the leg holds *now* and return it to Idle once
/// confirmed off the book. Read under the lock at this moment, not earlier: the
/// caller saved `markets.json` in between, and the engine may have replaced the
/// order meanwhile — that newer order is the one to cancel, and overwriting it
/// would leave it resting untracked. A placement still in flight is freed (it
/// cancels its own order when it lands). Unconfirmed cancels stay `Cancelling`;
/// the engine's 30s timer retries them (`placer::confirm_stuck_cancel`).
async fn cancel_leg_order(state: &WebState, id: &str) {
    let Some(order_id) = crate::engine::placer::settle_unplaced(&mut *state.engine.write().await, id) else {
        return;
    };
    if let Some((executor, _)) = state.engine_handle.get().await {
        let confirmed = executor.cancel_order_verified(&order_id).await.unwrap_or(false);
        if confirmed {
            let mut s = state.engine.write().await;
            if matches!(s.order_status.get(id), Some(OrderStatus::Cancelling { order_id: oid, .. }) if *oid == order_id)
            {
                s.order_status.insert(id.to_string(), OrderStatus::Idle);
            }
        }
    }
}

/// POST /markets/{id}/remove — cancels the leg's resting order, then removes it.
///
/// Order matters: the leg is paused first (the engine stops quoting it) and its
/// order claimed for cancelling, the order is cancelled and confirmed gone, and
/// only then is the leg removed, if it is still paused with no order. If the
/// cancel can't be confirmed the leg stays, paused and saved, so the order is
/// still tracked (the engine's timer retries the cancel, and the next start
/// cancels it) instead of being left on Polymarket with nothing watching it. A
/// leg resumed while its cancel ran is kept too.
pub async fn remove_market(
    State(state): State<WebState>,
    session: Session,
    Path(id): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return reply(&state, &session, &form.view, Some("Invalid session — reload the page.".into())).await;
    }
    // Set once at boot and never cleared, so one read serves the whole remove.
    let handle = state.engine_handle.get().await;
    match remove_leg(&state.engine, handle.as_ref().map(|(executor, _)| executor.as_ref()), &id).await {
        Removal::Missing => reply(&state, &session, &form.view, None).await,
        Removal::Kept(flash) => reply(&state, &session, &form.view, Some(flash)).await,
        Removal::Removed { unsubscribe } => {
            if !unsubscribe.is_empty() {
                if let Some((_, ws_cmd_tx)) = &handle {
                    let _ = ws_cmd_tx.send(WsCommand::Unsubscribe(unsubscribe)).await;
                }
            }
            reply(&state, &session, &form.view, None).await
        }
    }
}

/// What a remove came to.
#[derive(Debug)]
enum Removal {
    /// No such leg: it was already gone.
    Missing,
    /// Removed. `unsubscribe` lists its tokens that no other leg farms.
    Removed { unsubscribe: Vec<String> },
    /// Kept; the flash says why.
    Kept(String),
}

/// The remove itself (see [`remove_market`]), against any [`OrderApi`] so
/// tests can stand in for Polymarket. `api` is `None` while the engine isn't
/// running.
async fn remove_leg<A: OrderApi>(engine: &RwLock<AppState>, api: Option<&A>, id: &str) -> Removal {
    // 1. Pause in memory and take the leg's order, if any.
    let order_id = {
        let mut s = engine.write().await;
        if !s.configs.iter().any(|c| c.id == id) {
            return Removal::Missing;
        }
        for c in s.configs.iter_mut().filter(|c| c.id == id) {
            c.paused = true;
        }
        match (s.order_status.get(id), api) {
            // Already being cancelled (say, by a pause whose cancel is unconfirmed).
            (Some(OrderStatus::Cancelling { order_id, .. }), _) => Some(order_id.clone()),
            // Nothing to cancel it with, and no engine to place beside it: left
            // Live for step 2 to report.
            (Some(OrderStatus::Live { order_id, .. }), None) => Some(order_id.clone()),
            // Claimed the way pause and edit do: a live order becomes Cancelling,
            // which stops the engine placing for this leg even if it is resumed
            // meanwhile. An order still being placed is cancelled by the placing
            // task when it sees the status has moved on (see run_place).
            _ => crate::engine::placer::settle_unplaced(&mut s, id),
        }
    };

    // 2. Cancel it and confirm it is off the book.
    if let Some(order_id) = &order_id {
        let confirmed = match api {
            Some(api) => api.cancel_verified(order_id).await,
            None => Err(eyre::eyre!("the engine is not running")),
        };
        if let Err(why) = confirmed.and_then(|gone| if gone { Ok(()) } else { Err(eyre::eyre!("it is still live")) }) {
            tracing::warn!("Remove {id}: could not confirm cancel of {order_id}: {why}");
            let flash = match persist_configs(engine).await {
                Ok(()) => format!(
                    "Couldn't cancel its order on Polymarket ({why}). The market is paused and kept so the order stays \
                     tracked — try removing it again."
                ),
                Err(e) => format!("Couldn't cancel its order on Polymarket ({why}), and saving failed: {e}"),
            };
            return Removal::Kept(flash);
        }
    }

    // 3. Remove the leg, if it is still paused with no order.
    let mut s = engine.write().await;
    if let Some(order_id) = &order_id {
        // Only if the leg still holds that cancel: the state may have moved on
        // while it ran, and a newer order must not be overwritten.
        crate::engine::placer::free_if_cancelling(&mut s, id, order_id);
    }
    let Some(leg) = s.configs.iter().find(|c| c.id == id) else {
        return Removal::Missing;
    };
    // Resumed in another tab while the cancel ran: the engine may be quoting
    // it again, and deleting it would leave that order on Polymarket untracked.
    if !leg.paused || !matches!(s.order_status.get(id), None | Some(OrderStatus::Idle)) {
        return Removal::Kept(format!(
            "{} was resumed while it was being removed, so it was kept. Remove it again.",
            leg.label
        ));
    }
    let removed: Vec<_> = s.configs.iter().filter(|c| c.id == id).cloned().collect();
    s.configs.retain(|c| c.id != id);
    s.order_status.remove(id);
    s.place_failures.remove(id);
    s.place_retry_at.remove(id);
    s.place_attempt.remove(id);
    s.cancel_failures.remove(id);
    let unsubscribe: Vec<String> = removed
        .iter()
        .map(|c| c.token_id.clone())
        .filter(|tid| !s.configs.iter().any(|c| &c.token_id == tid))
        .collect();
    if let Err(e) = save_configs(&mut s) {
        // Keep it (paused, no order) rather than lose it from the file.
        for c in &removed {
            s.order_status.insert(c.id.clone(), OrderStatus::Idle);
        }
        s.configs.extend(removed);
        drop(s);
        tracing::error!("Failed to save markets.json after remove: {}", e);
        return Removal::Kept(format!("Its order was cancelled, but saving failed: {e}"));
    }
    Removal::Removed { unsubscribe }
}

/// Write the current market list to markets.json.
async fn persist_configs(engine: &RwLock<AppState>) -> Result<(), String> {
    save_configs(&mut *engine.write().await).map_err(|e| e.to_string())
}

/// Write the market list to markets.json, from the state itself: `&mut` means
/// the caller holds the engine's write lock, as the engine's own saves do. So
/// no change can land between taking this list and writing it, and an older
/// list is never written over a newer one. The file is small and rarely saved.
fn save_configs(s: &mut AppState) -> eyre::Result<()> {
    (s.markets_writer)(&s.markets_file, &s.configs)
}

/// The engine's (WS-fed) best bid for `token`, else a fresh REST read.
async fn best_bid_now(state: &WebState, token: &str) -> Option<Decimal> {
    let engine_bb = state.engine.read().await.books.get(token).and_then(|b| b.best_bid);
    if engine_bb.is_some() {
        return engine_bb;
    }
    match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(token)).await {
        Ok(Ok(b)) => b.best_bid,
        _ => None,
    }
}

/// Whether `cfg` (a leg with its edited size and distance) meets Polymarket's
/// minimum order where it would rest now, below the engine's live best bid.
/// Without a live book there is no price to check at, so the edit is refused.
/// A running (or starting) engine subscribes every configured leg, paused ones
/// too, so its missing book is still loading; with no engine, none will load.
async fn check_min_order(state: &WebState, cfg: &MarketConfig) -> Result<(), String> {
    let (phase, best_bid) = {
        let s = state.engine.read().await;
        (s.engine_phase.clone(), s.books.get(&cfg.token_id).and_then(|b| b.best_bid))
    };
    let Some(best_bid) = best_bid else {
        return Err(match phase {
            EnginePhase::Running | EnginePhase::Starting => {
                "The order book for this market isn't loaded yet — try again in a few seconds."
            }
            EnginePhase::AwaitingWallet | EnginePhase::Error => {
                "The bot isn't running, so there's no live order book to check the size against — start the bot \
                 (Settings), then edit."
            }
        }
        .into());
    };
    let min_shares = min_order_size(state, &cfg.token_id).await?;
    match min_order_shortfall(cfg, best_bid, min_shares) {
        Some((needed_usd, reason)) => Err(format!("{reason} Enter at least ${needed_usd:.2}.")),
        None => Ok(()),
    }
}

/// Polymarket's minimum order (shares) for `token`: cached (Start fills it from
/// its REST book), else read from the REST book. A failed read is an error, never
/// "no minimum".
async fn min_order_size(state: &WebState, token: &str) -> Result<Decimal, String> {
    state
        .caches
        .min_order_size
        .get_or_fetch(token, || async {
            match tokio::time::timeout(NETWORK_TIMEOUT, market_data::fetch_book(token)).await {
                Ok(Ok(book)) => Ok(book.min_order_size),
                _ => Err("Couldn't check Polymarket's minimum order size — try again.".to_string()),
            }
        })
        .await
}

/// The best bid a resumed leg's auto-pause limit restarts from: `None` when
/// the leg isn't paused or has no limit (nothing to restart).
async fn restart_price(state: &WebState, id: &str) -> Result<Option<Decimal>, String> {
    let leg = {
        let s = state.engine.read().await;
        s.configs.iter().find(|c| c.id == id).map(|c| (c.paused, c.max_volatility.is_some(), c.token_id.clone()))
    };
    match leg {
        Some((true, true, token)) => match best_bid_now(state, &token).await {
            Some(bb) => Ok(Some(bb)),
            None => Err("Couldn't read the current best bid, so auto-pause can't restart. Try again, or turn \
                         auto-pause off in Edit."
                .into()),
        },
        _ => Ok(None),
    }
}

/// Pause/resume one leg (`pausing` picks the direction). Resuming a paused
/// leg with an auto-pause limit restarts that limit from `restart_from` (the
/// current best bid; see `restart_price`), whether it was auto-paused or
/// paused by hand. Without this, a leg auto-paused after a move would stay
/// over its limit and never quote again.
async fn set_paused(state: &WebState, id: &str, pausing: bool, restart_from: Option<Decimal>) -> Result<(), String> {
    let order_to_cancel = {
        let mut s = state.engine.write().await;
        let Some(c) = s.configs.iter_mut().find(|c| c.id == id) else {
            return Ok(()); // already gone
        };
        let old = (c.paused, c.benchmark_bid);
        // Only a leg still paused with a limit now: a concurrent resume or edit may have got here first.
        let restarted = restart_from.filter(|_| !pausing && c.paused && c.max_volatility.is_some());
        if let Some(bb) = restarted {
            c.benchmark_bid = Some(bb);
        }
        c.paused = pausing;
        // A manual pause or resume replaces the "No balance" pause and its retry clock.
        s.low_balance_paused.remove(id);
        s.low_balance_since.remove(id);
        if !pausing {
            // Resuming is a fresh start: no leftover retry delay or failure count,
            // nor the two-sided rule's back-off (engine::pairing).
            s.place_retry_at.remove(id);
            s.place_failures.remove(id);
            s.place_outage.remove(id);
            s.pair_retry_at.remove(id);
            s.pair_pulls.remove(id);
            s.pair_alone_since.remove(id);
        }
        // Pausing settles whatever the leg holds once saved (cancel_leg_order):
        // a live order, or a placement still in flight.
        let order_to_cancel = pausing.then_some(());
        if let Err(e) = save_configs(&mut s) {
            // Undo what this call changed to the leg (still under the lock, so
            // nothing else has changed it since).
            if let Some(c) = s.configs.iter_mut().find(|c| c.id == id) {
                c.paused = old.0;
                if restarted.is_some() {
                    c.benchmark_bid = old.1;
                }
            }
            return Err(format!("Failed to save: {e}"));
        }
        order_to_cancel
    };

    if order_to_cancel.is_some() {
        cancel_leg_order(state, id).await;
    }
    // On resume, poke the quote loop so it re-quotes right away.
    if !pausing {
        state.quote_nudge.notify_one();
    }
    Ok(())
}

async fn pause_resume(
    state: WebState,
    session: Session,
    ids: Vec<String>,
    form: &MarketActionForm,
    pausing: bool,
) -> Response {
    if !verify_csrf(&session, &form.csrf).await {
        return reply(&state, &session, &form.view, Some("Invalid session — reload the page.".into())).await;
    }
    // A leg loaded paused for an invalid value stays paused until Edit fixes
    // it. Checked before anything else, so nothing changes or is saved, and no
    // best bid is looked up for a leg without a token.
    if !pausing {
        if let Some(problem) = invalid_leg(&state, &ids).await {
            return reply(&state, &session, &form.view, Some(format!("Can't resume {problem}"))).await;
        }
    }
    // Read every restart price before resuming anything, so a group resume
    // resumes all its legs or none (a lone leg would only wait on the
    // two-sided rule).
    let mut restart = Vec::with_capacity(ids.len());
    for id in &ids {
        match if pausing { Ok(None) } else { restart_price(&state, id).await } {
            Ok(p) => restart.push(p),
            Err(e) => return reply(&state, &session, &form.view, Some(e)).await,
        }
    }
    let mut flash = None;
    for (id, restart_from) in ids.iter().zip(restart) {
        if let Err(e) = set_paused(&state, id, pausing, restart_from).await {
            flash = Some(e);
            break;
        }
    }
    reply(&state, &session, &form.view, flash).await
}

/// The first of `ids` whose values are invalid, described for the user.
async fn invalid_leg(state: &WebState, ids: &[String]) -> Option<String> {
    let s = state.engine.read().await;
    s.configs.iter().filter(|c| ids.contains(&c.id)).find_map(|c| c.describe_problem())
}

pub async fn pause_market(
    State(state): State<WebState>,
    session: Session,
    Path(id): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    pause_resume(state, session, vec![id], &form, true).await
}

pub async fn resume_market(
    State(state): State<WebState>,
    session: Session,
    Path(id): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    pause_resume(state, session, vec![id], &form, false).await
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
    pause_resume(state, session, ids, &form, true).await
}

/// POST /markets/group/{cid}/resume
pub async fn resume_group(
    State(state): State<WebState>,
    session: Session,
    Path(cid): Path<String>,
    Form(form): Form<MarketActionForm>,
) -> Response {
    let ids = group_ids(&state, &cid).await;
    pause_resume(state, session, ids, &form, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Start refuses a distance Edit would refuse, with Edit's own message, so
    /// a leg can't be started at a distance it could then never be saved with.
    #[test]
    fn start_refuses_a_distance_over_the_edit_limit() {
        let edit_refusal = input::distance_cents("50.01").unwrap_err();
        // Le Pen NO: price 1¢ under a 55.7¢ best bid.
        assert_eq!(start_distance(dec!(0.557), dec!(0.01)), Err(edit_refusal.clone()));
        assert_eq!(start_distance(dec!(0.551), dec!(0.05)), Err(edit_refusal.clone()), "50.1¢");
        assert_eq!(start_distance(dec!(0.99), dec!(0.01)), Err(edit_refusal));
    }

    #[test]
    fn start_accepts_a_distance_up_to_the_edit_limit() {
        assert_eq!(start_distance(dec!(0.55), dec!(0.05)), Ok(dec!(0.50)), "exactly 50¢");
        assert_eq!(start_distance(dec!(0.557), dec!(0.507)), Ok(dec!(0.05)));
        assert_eq!(start_distance(dec!(0.0002), dec!(0.0001)), Ok(dec!(0.0001)), "the finest tick");
    }

    #[test]
    fn start_refuses_a_price_at_or_above_the_best_bid() {
        let refusal = Err("Your price must be below the current best bid.".to_string());
        assert_eq!(start_distance(dec!(0.40), dec!(0.40)), refusal);
        assert_eq!(start_distance(dec!(0.40), dec!(0.41)), refusal);
    }

    #[test]
    fn the_other_side_needs_room_below_its_best_bid() {
        assert_eq!(other_side_price(dec!(0.40), dec!(0.02), dec!(0.01)), Some(dec!(0.38)));
        assert_eq!(other_side_price(dec!(0.03), dec!(0.02), dec!(0.01)), Some(dec!(0.01)));
        assert_eq!(other_side_price(dec!(0.02), dec!(0.02), dec!(0.01)), None, "0¢ isn't a price");
        assert_eq!(other_side_price(dec!(0.01), dec!(0.03), dec!(0.01)), None, "below zero");
        assert_eq!(other_side_price(dec!(0.004), dec!(0.002), dec!(0.001)), Some(dec!(0.002)));
    }

    /// Shares are shown as the engine places them (2 decimals, not cut to whole
    /// shares), and an other leg with no room says so instead of a negative price.
    #[test]
    fn the_preview_shows_real_share_counts_and_no_room() {
        let mut t = PlacementPreviewTemplate::message(true, None);
        t.per_side_shares = dec!(49.6).normalize().to_string();
        t.per_side_size = super::super::dashboard::usd(dec!(19.84));
        t.min_size = "50".into();
        t.per_side_meets_min = false; // 49.6 < 50
        t.fill_risk = "low";
        t.other_leg = Some(OtherLeg {
            label: "No".into(),
            price_cents: "-1.0".into(),
            weight_pct: "0%".into(),
            meets_min: true,
            shares: "0".into(),
            no_room: true,
        });
        let html = t.render().unwrap();
        assert!(html.contains("49.6 sh · $19.84") && html.contains("under 50 min"), "{html}");
        assert!(html.contains("no room below its best bid") && !html.contains("-1.0¢"), "{html}");
    }

    fn two_outcome_market() -> MarketRef {
        MarketRef {
            condition_id: format!("0x{}", "ab".repeat(32)),
            question: "Q?".into(),
            group_item_title: None,
            market_slug: "q".into(),
            event_slug: "q".into(),
            image: None,
            outcomes: vec!["Yes".into(), "No".into()],
            token_ids: vec!["1".into(), "2".into()],
            outcome_prices: vec![dec!(0.4), dec!(0.6)],
            tick_size: dec!(0.01),
            rewards_min_size: None,
            rewards_max_spread: None,
            reward_pool: dec!(0),
            volume_24hr: None,
            liquidity: None,
            last_trade_price: None,
            one_day_price_change: None,
            end_date: None,
        }
    }

    fn book_at(best_bid: Decimal, min_order_size: Decimal) -> market_data::BookSnapshot {
        market_data::BookSnapshot {
            bids: vec![(best_bid, dec!(100))],
            asks: vec![],
            best_bid: Some(best_bid),
            best_ask: None,
            tick_size: dec!(0.01),
            min_order_size,
        }
    }

    fn start_settings(distance: Decimal, per_side: Decimal) -> StartSettings {
        StartSettings {
            distance,
            min_depth_between: dec!(0),
            order_size: per_side,
            expires_at: Utc::now() + chrono::Duration::days(7),
            max_volatility: None,
        }
    }

    /// Polymarket rejects an order under the market's minimum size (5 shares
    /// here), so Start refuses one before saving, checking each side at the
    /// price it would rest at and naming the per-side size that would do.
    #[test]
    fn start_refuses_a_per_side_size_below_the_minimum_order() {
        let mr = two_outcome_market();
        // Both sides at $2 ($1 per side), 2¢ below 40¢ and 62¢ best bids: 38¢ and 60¢.
        let (yes, no) = (book_at(dec!(0.40), dec!(5)), book_at(dec!(0.62), dec!(5)));
        let refusal = start_configs(&mr, 0, &yes, Some(&no), &start_settings(dec!(0.02), dec!(1))).unwrap_err();
        assert_eq!(
            refusal,
            "Too small for this market: $1.00 per side is 1.6 shares of No at 60¢; Polymarket's minimum order is 5 \
             shares. Enter at least $6.00 (split across both sides)."
        );

        // One side is big enough, the other isn't: $2.50 is 6.5 Yes shares but 4.1 No shares.
        let refusal = start_configs(&mr, 0, &yes, Some(&no), &start_settings(dec!(0.02), dec!(2.5))).unwrap_err();
        assert!(refusal.contains("$2.50 per side is 4.1 shares of No at 60¢"), "{refusal}");
        assert!(refusal.ends_with("Enter at least $6.00 (split across both sides)."), "{refusal}");

        // One side only.
        let refusal = start_configs(&mr, 0, &yes, None, &start_settings(dec!(0.02), dec!(1))).unwrap_err();
        assert!(refusal.contains("$1.00 per side is 2.6 shares of Yes at 38¢"), "{refusal}");
        assert!(refusal.ends_with("Enter at least $1.90."), "{refusal}");

        // A decimal price: 38.25¢ on a 0.1¢ tick rests at 38.2¢.
        let fine_tick = MarketRef { tick_size: dec!(0.001), ..two_outcome_market() };
        let refusal =
            start_configs(&fine_tick, 0, &book_at(dec!(0.402), dec!(5)), None, &start_settings(dec!(0.0195), dec!(1)))
                .unwrap_err();
        assert_eq!(
            refusal,
            "Too small for this market: $1.00 per side is 2.6 shares of Yes at 38.2¢; Polymarket's minimum order is 5 \
             shares. Enter at least $1.91."
        );
    }

    /// The amount a refusal says to enter, as typed in the form's size box.
    fn amount_to_enter(refusal: &str) -> Decimal {
        let after = refusal.split("Enter at least $").nth(1).unwrap();
        // "$1.90." or "$3.82 (split…": the amount, without the sentence's full stop.
        after.split_whitespace().next().unwrap().trim_end_matches('.').parse().unwrap()
    }

    /// Entering the amount a refusal names starts the farm: for both sides the
    /// form's size is the total, so it covers each side's minimum once split.
    #[test]
    fn the_amount_a_too_small_refusal_names_is_enough() {
        let fine_tick = MarketRef { tick_size: dec!(0.001), ..two_outcome_market() };
        let mr = two_outcome_market();
        let cases = [
            // (market, chosen side's book, other side's book, distance, total typed)
            (&fine_tick, book_at(dec!(0.402), dec!(5)), Some(book_at(dec!(0.402), dec!(5))), dec!(0.0195), dec!(2)),
            (&fine_tick, book_at(dec!(0.402), dec!(5)), None, dec!(0.0195), dec!(1)),
            (&mr, book_at(dec!(0.40), dec!(5)), Some(book_at(dec!(0.62), dec!(5))), dec!(0.02), dec!(5)),
            (&mr, book_at(dec!(0.40), dec!(5)), None, dec!(0.02), dec!(1)),
        ];
        for (market, book, other, distance, typed) in cases {
            let per_side = |total: Decimal| if other.is_some() { total / dec!(2) } else { total };
            let refusal = start_configs(market, 0, &book, other.as_ref(), &start_settings(distance, per_side(typed)))
                .unwrap_err();
            let enter = amount_to_enter(&refusal);
            if other.is_some() {
                assert!(refusal.ends_with("(split across both sides)."), "{refusal}");
            }
            let started = start_configs(market, 0, &book, other.as_ref(), &start_settings(distance, per_side(enter)));
            assert!(started.is_ok(), "entering ${enter} after {refusal:?} must start: {started:?}");
        }
        // The decimal-price example, both sides: $1.91 per side.
        let both = start_configs(
            &fine_tick,
            0,
            &book_at(dec!(0.402), dec!(5)),
            Some(&book_at(dec!(0.402), dec!(5))),
            &start_settings(dec!(0.0195), dec!(1)),
        )
        .unwrap_err();
        assert!(both.ends_with("Enter at least $3.82 (split across both sides)."), "{both}");
    }

    #[test]
    fn start_accepts_a_per_side_size_at_the_minimum_order() {
        let mr = two_outcome_market();
        let (yes, no) = (book_at(dec!(0.40), dec!(5)), book_at(dec!(0.62), dec!(5)));
        // Exactly 5 shares at 38¢ and at 60¢.
        let one = start_configs(&mr, 0, &yes, None, &start_settings(dec!(0.02), dec!(1.90))).unwrap();
        assert_eq!(one.len(), 1);
        let both = start_configs(&mr, 0, &yes, Some(&no), &start_settings(dec!(0.02), dec!(3))).unwrap();
        assert_eq!(both.len(), 2);
        // The decimal-price suggestion is enough: $1.91 at 38.2¢ is 5 shares.
        let fine_tick = MarketRef { tick_size: dec!(0.001), ..two_outcome_market() };
        let at_min = start_configs(
            &fine_tick,
            0,
            &book_at(dec!(0.402), dec!(5)),
            None,
            &start_settings(dec!(0.0195), dec!(1.91)),
        );
        assert!(at_min.is_ok(), "{at_min:?}");
        // No minimum in the book (0): nothing to check.
        let no_min = start_configs(&mr, 0, &book_at(dec!(0.40), dec!(0)), None, &start_settings(dec!(0.02), dec!(1)));
        assert!(no_min.is_ok(), "{no_min:?}");
    }

    /// Holders are valued at their own outcome's mid; names from Polymarket are
    /// escaped, and only a well-formed wallet becomes a profile link.
    #[test]
    fn holders_and_trades_render_safely() {
        let mr = two_outcome_market();
        let wallet = "0x25ff40795e25bb5f2a8dcc26f134ea81e224e0cf".to_string();
        let data = vec![
            market_flow::OutcomeHolders {
                outcome_index: 1,
                holders: vec![market_flow::Holder {
                    wallet: String::new(),
                    name: "<img src=x>".into(),
                    shares: dec!(1000),
                }],
            },
            market_flow::OutcomeHolders {
                outcome_index: 0,
                holders: vec![market_flow::Holder { wallet: wallet.clone(), name: "bob".into(), shares: dec!(2500) }],
            },
        ];
        let cols = holder_columns(&mr, [dec!(0.4), dec!(0.6)], &data);
        assert_eq!((cols[0].label.as_str(), cols[1].label.as_str()), ("Yes", "No"));
        assert_eq!((cols[0].rows[0].value.as_str(), cols[1].rows[0].value.as_str()), ("$1.0k", "$600"));
        assert_eq!(cols[0].mid_cents, "40.0");
        let html = HoldersTemplate { error: None, columns: cols }.render().unwrap();
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("&#60;img src=x&#62;") || html.contains("&lt;img src=x&gt;"), "{html}");
        assert_eq!(html.matches("https://polymarket.com/profile/").count(), 1);
        assert!(html.contains(&format!("https://polymarket.com/profile/{wallet}")));

        let trades = vec![market_flow::Trade {
            wallet: String::new(),
            name: "\"><script>".into(),
            side: market_flow::Side::Sell,
            outcome_index: 1,
            shares: dec!(7197.01),
            price: dec!(0.96374),
            timestamp: 1_795_170_038,
        }];
        let rows = trade_rows(&mr, &trades);
        assert_eq!((rows[0].outcome.as_str(), rows[0].price.as_str(), rows[0].usd.as_str()), ("No", "96.4", "$6.9k"));
        assert!(!rows[0].buy && rows[0].ts_iso.starts_with("2026-11-"));
        let html = TradesTemplate { error: None, filter: "$1k+".into(), rows }.render().unwrap();
        assert!(!html.contains("<script>") && html.contains("SELL"), "{html}");
        let empty = TradesTemplate { error: None, filter: "$100k+".into(), rows: vec![] }.render().unwrap();
        assert!(empty.contains("No $100k+ trades"), "{empty}");
    }

    #[test]
    fn min_depth_is_parsed_as_usd_not_cents() {
        // The quoter compares min_depth_between to USDC notional — "500" must mean
        // $500 (the old form divided by 100 → $5). Kept here as a regression guard
        // for the unit; the full bounds tests live in web/input.rs.
        assert_eq!(input::depth_usd("500"), Ok(dec!(500)));
        assert_eq!(input::depth_usd("$1,250.50"), Ok(dec!(1250.50)));
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

    #[test]
    fn sanitize_market_url_strips_control_chars_and_validates_scheme() {
        // Happy path: a plain https URL survives unchanged (apart from surrounding whitespace).
        let clean = sanitize_market_url("  https://polymarket.com/event/2024-elections/donald-trump  ").unwrap();
        assert_eq!(clean, "https://polymarket.com/event/2024-elections/donald-trump");

        // Control characters (NUL, BEL, ESC, newline) embedded by a paste are removed.
        let with_ctrl = sanitize_market_url("https://polymarket.com/event/\u{0007}trump\u{001b}").unwrap();
        assert_eq!(with_ctrl, "https://polymarket.com/event/trump");

        // Tab is a control char and gets stripped too — the slug extractor relies
        // on `/event/<slug>` splitting on `/`, so a stray tab mid-slug would
        // otherwise produce a phantom slug.
        let with_tab = sanitize_market_url("https://polymarket.com/event/tru\tmp").unwrap();
        assert_eq!(with_tab, "https://polymarket.com/event/trump");

        // `\trump`: the tab precedes "rump", so the cleaned slug is "rump",
        // not "rtrump" (reviewer case, PR #115).
        let leading_tab = sanitize_market_url("https://polymarket.com/event/\trump").unwrap();
        assert_eq!(leading_tab, "https://polymarket.com/event/rump");

        // Share links: query strings and fragments are stripped before the
        // slug extractor sees them (e.g. Polymarket ?tid= tracking params).
        let with_query = sanitize_market_url("https://polymarket.com/event/some-market?tid=123").unwrap();
        assert_eq!(with_query, "https://polymarket.com/event/some-market");
        let with_frag = sanitize_market_url("https://polymarket.com/event/some-market#top").unwrap();
        assert_eq!(with_frag, "https://polymarket.com/event/some-market");
        // Everything from the first '?' or '#' goes, whatever order they appear in.
        let with_both = sanitize_market_url("https://polymarket.com/event/x?a=1#b").unwrap();
        assert_eq!(with_both, "https://polymarket.com/event/x");
        // Control characters and query strings combined.
        let mixed = sanitize_market_url("https://polymarket.com/event/\trump?src=share\n").unwrap();
        assert_eq!(mixed, "https://polymarket.com/event/rump");

        // A bare slug is read as an event page link.
        assert_eq!(
            sanitize_market_url(" lol-blg-gen-2026-11-14 ").unwrap(),
            "https://polymarket.com/event/lol-blg-gen-2026-11-14"
        );
        // The link from the bug report: query and fragment dropped, path kept.
        assert_eq!(
            sanitize_market_url(
                "https://polymarket.com/esports/league-of-legends/worlds/lol-blg-gen-2026-11-14?r=earnfees#L97Nndk"
            )
            .unwrap(),
            "https://polymarket.com/esports/league-of-legends/worlds/lol-blg-gen-2026-11-14"
        );

        // Empty / whitespace-only input is rejected before the network call.
        assert!(sanitize_market_url("").is_err());
        assert!(sanitize_market_url("   \n\t  ").is_err());

        // Only http(s) schemes are accepted.
        assert!(sanitize_market_url("javascript:alert(1)").is_err());
        assert!(sanitize_market_url("file:///etc/passwd").is_err());
        assert!(sanitize_market_url("data:text/html,<script>").is_err());

        // http (not just https) is accepted — only the extracted slug, not the
        // pasted URL, is ever sent to Gamma.
        assert!(sanitize_market_url("http:polymarket.com/event/x").is_ok());

        // A dotted head is a host:port, not a scheme — accepted and normalised.
        let host_port = sanitize_market_url("polymarket.com:443").unwrap();
        assert_eq!(host_port, "https://polymarket.com:443");
        assert!(sanitize_market_url("http://polymarket.com/event/x").is_ok());
        // Scheme matching is case-insensitive.
        assert!(sanitize_market_url("HTTPS://polymarket.com/event/x").is_ok());

        // Scheme-less URLs are accepted and prepended with https://.
        let schemeless = sanitize_market_url("polymarket.com/event/some-market").unwrap();
        assert_eq!(schemeless, "https://polymarket.com/event/some-market");

        // Scheme-less with trailing slash is normalised.
        let with_slash = sanitize_market_url("polymarket.com/event/x/").unwrap();
        assert_eq!(with_slash, "https://polymarket.com/event/x/");
    }

    #[test]
    fn browse_says_ended_for_listed_markets_past_their_end_date() {
        assert_eq!(browse_ends(Utc::now() - chrono::Duration::hours(1)), "ended");
        assert_eq!(browse_ends(Utc::now() + chrono::Duration::days(3) + chrono::Duration::minutes(5)), "3d 0h");
    }

    mod remove {
        use std::sync::Arc;
        use std::time::Duration;

        use rust_decimal_macros::dec;
        use tokio::sync::Notify;

        use super::super::{remove_leg, Removal};
        use crate::engine::test_support::{env, leg, Env, Fake};
        use crate::types::OrderStatus;

        fn live(order_id: &str) -> OrderStatus {
            OrderStatus::Live { order_id: order_id.into(), price: dec!(0.4) }
        }

        /// One leg "a" holding `status`.
        async fn leg_with(status: OrderStatus) -> Env {
            let e = env(vec![leg("a", dec!(0.01))]);
            e.state.write().await.order_status.insert("a".into(), status);
            e
        }

        /// A Polymarket whose cancel replies only once the gate is notified.
        fn slow_cancel() -> (Arc<Fake>, Arc<Notify>) {
            let fake = Fake::new(dec!(0.01), dec!(0.01));
            let gate = Arc::new(Notify::new());
            *fake.verify_gate.lock().unwrap() = Some(Arc::clone(&gate));
            (fake, gate)
        }

        /// Waits until the remove has sent its cancel of `order_id`.
        async fn cancel_sent(fake: &Fake, order_id: &str) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !fake.cancelled.lock().unwrap().iter().any(|o| o == order_id) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        }

        fn saved(e: &Env) -> String {
            std::fs::read_to_string(e.dir.join("markets.json")).unwrap()
        }

        /// The bug: Resume in another tab while Remove waits on a slow cancel.
        /// The engine quotes the resumed leg again (a new order), then the
        /// remove finished and deleted the leg, leaving that new order resting
        /// on Polymarket with nothing tracking it.
        #[tokio::test]
        async fn a_leg_resumed_during_its_remove_keeps_its_new_order_tracked() {
            let e = leg_with(live("old")).await;
            let (fake, gate) = slow_cancel();
            let remove = tokio::spawn({
                let state = Arc::clone(&e.state);
                let fake = Arc::clone(&fake);
                async move { remove_leg(&state, Some(fake.as_ref()), "a").await }
            });
            cancel_sent(&fake, "old").await;
            {
                let mut s = e.state.write().await;
                // Resume, as set_paused(false) does...
                s.configs[0].paused = false;
                // ...and the engine, once the old order is off the book, places a new one.
                s.order_status.insert("a".into(), OrderStatus::Idle);
                s.order_status.insert("a".into(), live("new"));
            }
            gate.notify_one();
            let outcome = remove.await.unwrap();

            let s = e.state.read().await;
            assert!(s.configs.iter().any(|c| c.id == "a"), "the leg holding the new order is kept: {outcome:?}");
            assert_eq!(s.order_status.get("a"), Some(&live("new")), "the new order is still tracked");
            assert!(
                matches!(&outcome, Removal::Kept(flash) if flash.contains("was resumed while it was being removed")),
                "{outcome:?}"
            );
            assert!(saved(&e).contains("\"id\": \"a\""), "kept in markets.json too");
        }

        /// While the remove's cancel runs the leg is Cancelling, so resuming it
        /// can't get a second order placed beside the old one. Once the cancel
        /// confirms, the resumed leg is freed and kept.
        #[tokio::test]
        async fn a_leg_resumed_during_its_remove_is_not_placed_until_the_cancel_confirms() {
            let e = leg_with(live("old")).await;
            let (fake, gate) = slow_cancel();
            let remove = tokio::spawn({
                let state = Arc::clone(&e.state);
                let fake = Arc::clone(&fake);
                async move { remove_leg(&state, Some(fake.as_ref()), "a").await }
            });
            cancel_sent(&fake, "old").await;
            {
                let mut s = e.state.write().await;
                s.configs[0].paused = false;
                assert!(
                    matches!(s.order_status.get("a"), Some(OrderStatus::Cancelling { order_id, .. }) if order_id == "old")
                );
                let placed = crate::engine::placer::begin_place(&mut s, "a", dec!(0.39), tokio::time::Instant::now());
                assert_eq!(placed, None, "nothing placed beside the order being cancelled");
            }
            gate.notify_one();
            let outcome = remove.await.unwrap();

            let mut s = e.state.write().await;
            assert!(matches!(&outcome, Removal::Kept(flash) if flash.contains("leg a was resumed")), "{outcome:?}");
            assert!(s.configs.iter().any(|c| c.id == "a" && !c.paused), "kept, resumed");
            assert_eq!(s.order_status.get("a"), Some(&OrderStatus::Idle));
            let placed = crate::engine::placer::begin_place(&mut s, "a", dec!(0.39), tokio::time::Instant::now());
            assert!(placed.is_some(), "free to quote again");
        }

        #[tokio::test]
        async fn removing_a_leg_cancels_its_order_then_deletes_it() {
            let e = leg_with(live("old")).await;
            let fake = Fake::new(dec!(0.01), dec!(0.01));
            let outcome = remove_leg(&e.state, Some(fake.as_ref()), "a").await;

            assert!(
                matches!(&outcome, Removal::Removed { unsubscribe } if unsubscribe == &[crate::engine::test_support::TOKEN]),
                "{outcome:?}"
            );
            assert_eq!(*fake.cancelled.lock().unwrap(), vec!["old".to_string()]);
            let s = e.state.read().await;
            assert!(s.configs.is_empty() && !s.order_status.contains_key("a"));
            assert!(!saved(&e).contains("\"id\": \"a\""), "{}", saved(&e));
        }

        /// A pause whose cancel is still unconfirmed left the leg Cancelling:
        /// removing it cancels that order and deletes the leg.
        #[tokio::test]
        async fn removing_a_leg_still_cancelling_cancels_that_order() {
            let e =
                leg_with(OrderStatus::Cancelling { order_id: "old".into(), since: std::time::Instant::now() }).await;
            e.state.write().await.configs[0].paused = true;
            let fake = Fake::new(dec!(0.01), dec!(0.01));
            let outcome = remove_leg(&e.state, Some(fake.as_ref()), "a").await;

            assert!(matches!(outcome, Removal::Removed { .. }), "{outcome:?}");
            assert_eq!(*fake.cancelled.lock().unwrap(), vec!["old".to_string()]);
            assert!(e.state.read().await.configs.is_empty());
        }

        /// An unconfirmed cancel keeps the leg, paused and saved, still holding
        /// its order as Cancelling: the engine's timer retries that cancel and
        /// places nothing for the leg meanwhile.
        #[tokio::test]
        async fn an_unconfirmed_cancel_keeps_the_leg_paused_and_its_order_tracked() {
            let e = leg_with(live("old")).await;
            let fake = Fake::new(dec!(0.01), dec!(0.01));
            fake.verify.lock().unwrap().push_back(Err("checking order old timed out after 10s".into()));
            let outcome = remove_leg(&e.state, Some(fake.as_ref()), "a").await;

            assert!(
                matches!(&outcome, Removal::Kept(flash) if flash.contains("Couldn't cancel its order on Polymarket")),
                "{outcome:?}"
            );
            let s = e.state.read().await;
            assert!(s.configs.iter().any(|c| c.id == "a" && c.paused), "kept, paused");
            assert!(
                matches!(s.order_status.get("a"), Some(OrderStatus::Cancelling { order_id, .. }) if order_id == "old"),
                "still tracked"
            );
            assert!(saved(&e).contains("\"paused\": true"), "{}", saved(&e));
        }
    }
}
