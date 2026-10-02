//! Market data for the interactive market view: order book, price history,
//! midpoint — plus the pure "farming math" (does this placement qualify for
//! rewards? what's the fill risk? where should we suggest placing?).
//!
//! All reads go through a cached **unauthenticated** CLOB client — `order_book`/
//! `price_history`/`midpoint` are public market-data endpoints (on the SDK's
//! generic client impl, not the authenticated one), so this works regardless of
//! whether the trading engine is running. No secrets involved.
//!
//! The pure helpers (`evaluate_placement`, `suggest_placement`) carry no
//! network/IO so they're unit-tested below.

use std::str::FromStr;
use std::sync::LazyLock;

use alloy::primitives::U256;
use eyre::Result;
use polymarket_client_sdk_v2::clob::types::request::{
    MidpointRequest, OrderBookSummaryRequest, PriceHistoryRequest,
};
use polymarket_client_sdk_v2::clob::types::Interval;
use polymarket_client_sdk_v2::clob::{Client, Config as ClobConfig};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::engine::orderbook::TokenBook;

const CLOB_URL: &str = "https://clob.polymarket.com";

/// Cached unauthenticated client (connection reuse across the ~1.5s book polls).
/// `Client::new` is sync + only fails on a bad URL — a static const URL — so the
/// `expect` can't fire in practice.
static CLIENT: LazyLock<Client> =
    LazyLock::new(|| Client::new(CLOB_URL, ClobConfig::default()).expect("valid CLOB URL"));

fn token_u256(token_id: &str) -> Result<U256> {
    U256::from_str(token_id).map_err(|e| eyre::eyre!("invalid token_id {token_id}: {e}"))
}

// ── Network reads ───────────────────────────────────────────────────────────

/// Authoritative midpoint (the API uses last-trade when the spread is wide).
pub async fn fetch_midpoint(token_id: &str) -> Result<Decimal> {
    let req = MidpointRequest::builder().token_id(token_u256(token_id)?).build();
    Ok(CLIENT.midpoint(&req).await?.mid)
}

/// One unix-second timestamped price point per `(t, p)`.
///
/// `fidelity` (minutes per point) matters: Polymarket returns an EMPTY history
/// for a 1-week range without it, so each range gets an explicit resolution.
pub async fn fetch_history(token_id: &str, interval: Interval) -> Result<Vec<(i64, Decimal)>> {
    let fidelity = match interval {
        Interval::OneDay => Some(5),
        Interval::OneWeek => Some(30),
        _ => None,
    };
    let req = PriceHistoryRequest::builder()
        .market(token_u256(token_id)?)
        .time_range(interval)
        .maybe_fidelity(fidelity)
        .build();
    Ok(CLIENT.price_history(&req).await?.history.into_iter().map(|p| (p.t, p.p)).collect())
}

/// A normalized snapshot of one token's book — bids sorted best-first
/// (descending price), asks best-first (ascending price), regardless of the
/// order the API returns them in.
pub struct BookSnapshot {
    pub bids: Vec<(Decimal, Decimal)>,
    pub asks: Vec<(Decimal, Decimal)>,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub tick_size: Decimal,
    pub min_order_size: Decimal,
}

pub async fn fetch_book(token_id: &str) -> Result<BookSnapshot> {
    let req = OrderBookSummaryRequest::builder().token_id(token_u256(token_id)?).build();
    let resp = CLIENT.order_book(&req).await?;

    let mut bids: Vec<(Decimal, Decimal)> = resp.bids.iter().map(|l| (l.price, l.size)).collect();
    let mut asks: Vec<(Decimal, Decimal)> = resp.asks.iter().map(|l| (l.price, l.size)).collect();
    bids.sort_by_key(|(p, _)| std::cmp::Reverse(*p)); // best (highest) bid first
    asks.sort_by_key(|(p, _)| *p); // best (lowest) ask first

    Ok(BookSnapshot {
        best_bid: bids.first().map(|(p, _)| *p),
        best_ask: asks.first().map(|(p, _)| *p),
        tick_size: Decimal::from(resp.tick_size),
        min_order_size: resp.min_order_size,
        bids,
        asks,
    })
}

// ── Farming math (pure, tested) ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRisk {
    Low,
    Medium,
    High,
}

impl FillRisk {
    pub fn label(self) -> &'static str {
        match self {
            FillRisk::Low => "low",
            FillRisk::Medium => "medium",
            FillRisk::High => "high",
        }
    }
}

/// Result of evaluating a candidate resting BUY at `your_price` for `order_size`.
/// `max_spread_cents` and `min_size` come straight from the market's reward
/// config; `both_sides` halves the size across the two outcome legs.
pub struct PlacementEval {
    pub qualifies: bool,
    /// Distance below the midpoint, in cents (negative if at/above midpoint).
    pub cents_from_mid: Decimal,
    /// USD that actually rests on this side ( = order_size, or /2 if both_sides).
    pub per_side_size: Decimal,
    /// Shares that rest on this side ( = per-side USD / price). The reward
    /// minimum is a SHARE count, so this is what `per_side_meets_min` checks.
    pub per_side_shares: Decimal,
    pub per_side_meets_min: bool,
    pub fill_risk: FillRisk,
    /// Polymarket's per-order scoring weight at this distance from the
    /// midpoint, ((v − s) / v)² — 1.0 at the midpoint, 0 at the band edge.
    /// Before the single/two-sided adjustment (see [`effective_weight`]).
    pub score_weight: Decimal,
}

pub fn evaluate_placement(
    book: &BookSnapshot,
    midpoint: Decimal,
    max_spread_cents: Decimal,
    min_size: Decimal,
    your_price: Decimal,
    order_size: Decimal,
    both_sides: bool,
) -> PlacementEval {
    let cents_from_mid = (midpoint - your_price) * dec!(100);
    let per_side_size = if both_sides { order_size / dec!(2) } else { order_size };

    // The reward minimum is a SHARE count, not USD — compare shares
    // (per-side USD / price), not the dollar amount. (Was a units bug: e.g. at
    // 50c, "$200" is 400 shares, but comparing 200 >= 200 mis-qualified.)
    let per_side_shares = if your_price > dec!(0) {
        (per_side_size / your_price).round_dp(2)
    } else {
        dec!(0)
    };
    let per_side_meets_min = per_side_shares >= min_size;

    // Reward-qualifying: within max_spread of the midpoint (we farm the bid
    // side, so the order sits at or below the midpoint) AND each resting order
    // meets the minimum size.
    let within_band = cents_from_mid >= dec!(0) && cents_from_mid <= max_spread_cents;
    let qualifies = within_band && per_side_meets_min;

    PlacementEval {
        qualifies,
        cents_from_mid,
        per_side_size,
        per_side_shares,
        per_side_meets_min,
        // Fill risk compares USDC depth-ahead to USD size, so it stays in USD.
        fill_risk: fill_risk(book, your_price, per_side_size),
        score_weight: score_weight(midpoint, your_price, max_spread_cents),
    }
}

/// Heuristic: how exposed is a resting BUY at `your_price` to being filled?
/// Driven by how much bid depth sits *ahead* of it (higher-priced bids fill
/// first). At/above the best bid = front of the queue = high. Thresholds are
/// deliberately simple and can be tuned later.
fn fill_risk(book: &BookSnapshot, your_price: Decimal, size: Decimal) -> FillRisk {
    let best_bid = match book.best_bid {
        Some(b) => b,
        None => return FillRisk::High,
    };
    if your_price >= best_bid {
        return FillRisk::High;
    }
    // USDC depth strictly above your price, up to and including best bid.
    let depth_ahead: Decimal = book
        .bids
        .iter()
        .filter(|(p, _)| *p > your_price && *p <= best_bid)
        .map(|(p, s)| p * s)
        .sum();
    if size <= dec!(0) {
        return FillRisk::Medium;
    }
    let ratio = depth_ahead / size;
    if ratio >= dec!(3) {
        FillRisk::Low
    } else if ratio >= dec!(1) {
        FillRisk::Medium
    } else {
        FillRisk::High
    }
}

// ── Reward scoring model ────────────────────────────────────────────────────
// Mirrors Polymarket's liquidity-rewards rules (docs: programs/liquidity-rewards):
//   * each order scores S(v, s) = ((v − s) / v)² · size, where v is the market's
//     max spread and s the order's distance from the midpoint (both in cents);
//   * with the midpoint in [0.10, 0.90], one-sided liquidity still scores but
//     divided by c = 3; outside that range it must be two-sided to score at all.

/// The single-sided scaling factor `c` (currently 3.0 on all markets).
pub const SINGLE_SIDED_FACTOR: Decimal = dec!(3);

/// Per-order scoring weight in [0, 1]: `((v − s) / v)²`, 0 outside the band.
pub fn score_weight(midpoint: Decimal, price: Decimal, max_spread_cents: Decimal) -> Decimal {
    if max_spread_cents <= dec!(0) {
        return dec!(0);
    }
    let s = ((midpoint - price) * dec!(100)).abs();
    if s >= max_spread_cents {
        return dec!(0);
    }
    let r = (max_spread_cents - s) / max_spread_cents;
    r * r
}

/// Whether one-sided liquidity can score at this midpoint (in [0.10, 0.90]).
pub fn single_sided_allowed(midpoint: Decimal) -> bool {
    midpoint >= dec!(0.10) && midpoint <= dec!(0.90)
}

/// Effective weight after Polymarket's two-sided rule, given this leg's weight
/// and the complementary leg's (`None` / 0 = one-sided):
/// `max(min(a, b), max(a, b) / c)` when single-sided is allowed, else `min(a, b)`.
pub fn effective_weight(midpoint: Decimal, this_leg: Decimal, other_leg: Option<Decimal>) -> Decimal {
    let other = other_leg.unwrap_or(dec!(0));
    let lo = this_leg.min(other);
    if single_sided_allowed(midpoint) {
        lo.max(this_leg.max(other) / SINGLE_SIDED_FACTOR)
    } else {
        lo
    }
}

/// A named starting placement offered as a one-click chip.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub key: &'static str,
    pub label: &'static str,
    pub price: Decimal,
    /// Scoring weight at `price` (before the two-sided adjustment).
    pub weight: Decimal,
}

/// Starting placements trading reward weight against fill risk: the lowest
/// tick-aligned price reaching each target weight (25% / 50% / 75%), kept
/// strictly below the best bid (the engine pegs below it). Presets that
/// collapse onto the same price, or would score nothing, are dropped.
pub fn placement_presets(
    midpoint: Decimal,
    max_spread_cents: Decimal,
    tick: Decimal,
    best_bid: Option<Decimal>,
) -> Vec<Preset> {
    // 1 − √target, precomputed (rust_decimal has no sqrt without `maths`).
    const TARGETS: [(&str, &str, Decimal); 3] = [
        ("safer", "Safer", dec!(0.5)),             // 25% weight
        ("balanced", "Balanced", dec!(0.29289322)), // 50%
        ("tight", "Tight", dec!(0.13397460)),       // 75%
    ];
    if max_spread_cents <= dec!(0) || tick <= dec!(0) {
        return Vec::new();
    }
    let mut out: Vec<Preset> = Vec::new();
    for (key, label, one_minus_sqrt) in TARGETS {
        let max_s = max_spread_cents * one_minus_sqrt / dec!(100);
        let mut price = ((midpoint - max_s) / tick).ceil() * tick;
        if let Some(bb) = best_bid {
            if price >= bb {
                price = bb - tick;
            }
        }
        let price = price.normalize();
        if price <= dec!(0) || price >= dec!(1) {
            continue;
        }
        let weight = score_weight(midpoint, price, max_spread_cents);
        if weight <= dec!(0) || out.iter().any(|p| p.price == price) {
            continue;
        }
        out.push(Preset { key, label, price, weight });
    }
    out
}

/// Midpoint from the top of book, when both sides exist.
pub fn book_midpoint(book: &BookSnapshot) -> Option<Decimal> {
    match (book.best_bid, book.best_ask) {
        (Some(b), Some(a)) => Some((b + a) / dec!(2)),
        _ => None,
    }
}

/// Sorted snapshot of a live [`TokenBook`] (the engine's / book hub's WS-fed
/// book), so the same ladder + placement math runs on either source.
pub fn snapshot_from_token_book(book: &TokenBook, tick_size: Decimal) -> BookSnapshot {
    let mut bids: Vec<(Decimal, Decimal)> = book.bids.iter().map(|(p, s)| (*p, *s)).collect();
    let mut asks: Vec<(Decimal, Decimal)> = book.asks.iter().map(|(p, s)| (*p, *s)).collect();
    bids.sort_by_key(|(p, _)| std::cmp::Reverse(*p));
    asks.sort_by_key(|(p, _)| *p);
    BookSnapshot {
        best_bid: book.best_bid.or_else(|| bids.first().map(|(p, _)| *p)),
        best_ask: book.best_ask.or_else(|| asks.first().map(|(p, _)| *p)),
        tick_size,
        min_order_size: dec!(0),
        bids,
        asks,
    }
}

// ── Order-book ladder view-model (presentation) ──────────────────────────────

pub struct LadderRow {
    pub price_cents: String,  // "34.0"
    pub price_raw: String,    // "0.34" — the value click-to-set writes
    pub size: String,         // shares at this level, compact
    pub total: String,        // cumulative shares from the best level, compact
    pub total_usd: String,    // cumulative USD depth (Σ price × size) from the best level
    pub depth_pct: u32,       // 0-100, cumulative-depth bar width
    pub side: &'static str,   // "ask" | "bid"
    pub in_band: bool,        // within the reward-qualifying zone
    pub is_best: bool,
    /// One of YOUR live engine orders rests at this level (solid marker; the
    /// dashed placement preview is drawn client-side).
    pub is_live_mine: bool,
}

pub struct Ladder {
    pub asks: Vec<LadderRow>, // displayed high→low (best ask is last)
    pub bids: Vec<LadderRow>, // displayed high→low (best bid is first)
    pub midpoint_cents: String,
    pub spread_cents: String,
    pub best_bid_cents: String,
    pub best_ask_cents: String,
    pub band_lo_cents: String, // reward-zone lower bound (display)
    pub band_hi_cents: String, // reward-zone upper bound (display)
    pub in_zone_usdc: String,  // qualifying liquidity in the band (display, e.g. "$9.5k")
    pub in_zone_raw: String,   // same, plain number for the client-side "your share" calc
    /// Raw top-of-book + midpoint (price units) for the client-side reward math.
    pub best_bid_raw: String,
    pub best_ask_raw: String,
    pub midpoint_raw: String,
    pub has_book: bool,
}

/// Build the display ladder: top `per_side` levels each side, cumulative-depth
/// bars (shares, normalized across shown levels), reward band shaded. Reward band
/// = within `max_spread_cents` of the midpoint on either side. `live_mine` are
/// the prices of your live engine orders on this token (marked on the ladder).
/// `group` is the price-bucket size for the grouping control (== tick → raw).
pub fn build_ladder(
    book: &BookSnapshot,
    midpoint: Decimal,
    max_spread_cents: Decimal,
    per_side: usize,
    live_mine: &[Decimal],
    group: Decimal,
) -> Ladder {
    let band = max_spread_cents / dec!(100);
    let band_lo = midpoint - band;
    let band_hi = midpoint + band;

    // Aggregate the full book into price buckets of `group` (the grouping
    // control): bids floor toward the spread, asks ceil. Levels are already
    // sorted best-first, so same-bucket entries are consecutive. group == tick
    // (or 0) → identity.
    let bucketize = |levels: &[(Decimal, Decimal)], is_bid: bool| -> Vec<(Decimal, Decimal)> {
        if group <= dec!(0) {
            return levels.to_vec();
        }
        let mut out: Vec<(Decimal, Decimal)> = Vec::new();
        for (p, s) in levels {
            let b = if is_bid { (p / group).floor() * group } else { (p / group).ceil() * group };
            match out.last_mut() {
                Some(last) if last.0 == b => last.1 += *s,
                _ => out.push((b, *s)),
            }
        }
        out
    };
    let bid_all = bucketize(&book.bids, true);
    let ask_all = bucketize(&book.asks, false);
    // Your live orders are bids — bucket them the same way bid levels are.
    let mine: Vec<Decimal> = live_mine.iter()
        .map(|y| if group > dec!(0) { ((y / group).floor() * group).normalize() } else { y.normalize() })
        .collect();

    // bids best-first (sorted desc); asks best-first (sorted asc).
    let bid_levels: Vec<(Decimal, Decimal)> = bid_all.iter().take(per_side).copied().collect();
    let ask_levels: Vec<(Decimal, Decimal)> = ask_all.iter().take(per_side).copied().collect();

    // "Total" column = cumulative shares from the best level outward. The depth
    // bar shows each level's OWN size (normalized to the largest single level
    // shown) — so big resting walls stand out and stay readable even when the
    // book is deep and scrolled (a cumulative bar would shrink the near-spread
    // levels to invisible slivers once many levels are shown).
    let cum = |levels: &[(Decimal, Decimal)]| -> Vec<Decimal> {
        let mut acc = dec!(0);
        levels.iter().map(|(_, s)| { acc += s; acc }).collect()
    };
    let bid_cum = cum(&bid_levels);
    let ask_cum = cum(&ask_levels);
    // Cumulative USD depth: what it would cost / is resting from the best level out.
    let cum_usd = |levels: &[(Decimal, Decimal)]| -> Vec<Decimal> {
        let mut acc = dec!(0);
        levels.iter().map(|(p, s)| { acc += p * s; acc }).collect()
    };
    let bid_usd = cum_usd(&bid_levels);
    let ask_usd = cum_usd(&ask_levels);
    let max_level = bid_levels.iter().chain(ask_levels.iter())
        .map(|(_, s)| *s).fold(dec!(0), Decimal::max);

    let pct = |s: Decimal| -> u32 {
        if max_level <= dec!(0) { 0 } else { ((s / max_level) * dec!(100)).round().to_u32().unwrap_or(0).min(100) }
    };
    let row = |p: Decimal, s: Decimal, cumv: Decimal, usd: Decimal, side: &'static str, is_best: bool| LadderRow {
        price_cents: format!("{:.1}", p * dec!(100)),
        price_raw: p.normalize().to_string(),
        size: fmt_shares(s),
        total: fmt_shares(cumv),
        total_usd: fmt_usd(usd),
        depth_pct: pct(s),
        side,
        in_band: p > band_lo && p < band_hi,
        is_best,
        is_live_mine: side == "bid" && mine.contains(&p.normalize()),
    };

    // asks displayed high→low so the best ask sits just above the midpoint line.
    let asks: Vec<LadderRow> = ask_levels.iter().zip(ask_cum.iter()).zip(ask_usd.iter()).enumerate()
        .map(|(i, (((p, s), c), u))| row(*p, *s, *c, *u, "ask", i == 0))
        .rev()
        .collect();
    let bids: Vec<LadderRow> = bid_levels.iter().zip(bid_cum.iter()).zip(bid_usd.iter()).enumerate()
        .map(|(i, (((p, s), c), u))| row(*p, *s, *c, *u, "bid", i == 0))
        .collect();

    let spread = match (book.best_bid, book.best_ask) {
        (Some(b), Some(a)) => format!("{:.1}", (a - b) * dec!(100)),
        _ => "—".to_string(),
    };
    let cents = |o: Option<Decimal>| o.map(|p| format!("{:.1}", p * dec!(100))).unwrap_or_else(|| "—".to_string());

    // Qualifying liquidity in the reward band — the full book within
    // [mid − max_spread, mid + max_spread], both sides, in USDC notional. A
    // factual read on how crowded the reward zone already is.
    let in_zone: Decimal = book.bids.iter().chain(book.asks.iter())
        .filter(|(p, _)| *p > band_lo && *p < band_hi)
        .map(|(p, s)| p * s)
        .sum();

    Ladder {
        has_book: !asks.is_empty() || !bids.is_empty(),
        asks,
        bids,
        midpoint_cents: format!("{:.1}", midpoint * dec!(100)),
        spread_cents: spread,
        best_bid_cents: cents(book.best_bid),
        best_ask_cents: cents(book.best_ask),
        band_lo_cents: format!("{:.1}", band_lo.max(dec!(0)) * dec!(100)),
        band_hi_cents: format!("{:.1}", band_hi.min(dec!(1)) * dec!(100)),
        in_zone_usdc: fmt_usd(in_zone),
        in_zone_raw: in_zone.round_dp(2).to_string(),
        best_bid_raw: book.best_bid.map(|p| p.normalize().to_string()).unwrap_or_default(),
        best_ask_raw: book.best_ask.map(|p| p.normalize().to_string()).unwrap_or_default(),
        midpoint_raw: midpoint.normalize().to_string(),
    }
}

fn fmt_shares(s: Decimal) -> String {
    if s >= dec!(1000) {
        format!("{:.1}k", (s / dec!(1000)))
    } else {
        format!("{:.0}", s)
    }
}

pub fn fmt_usd(v: Decimal) -> String {
    if v >= dec!(1000000) {
        format!("${:.1}M", v / dec!(1000000))
    } else if v >= dec!(1000) {
        format!("${:.1}k", v / dec!(1000))
    } else {
        format!("${:.0}", v)
    }
}

/// Low / high / first→last change (all in cents, formatted) over the given
/// price history — for the chart caption. `None` if there's too little history.
pub fn history_range(points: &[(i64, Decimal)]) -> Option<(String, String, String)> {
    if points.len() < 2 {
        return None;
    }
    let mut lo = points[0].1;
    let mut hi = points[0].1;
    for (_, p) in points {
        if *p < lo { lo = *p; }
        if *p > hi { hi = *p; }
    }
    let change = (points[points.len() - 1].1 - points[0].1) * dec!(100);
    let sign = if change >= dec!(0) { "+" } else { "" };
    Some((
        format!("{:.1}", lo * dec!(100)),
        format!("{:.1}", hi * dec!(100)),
        format!("{sign}{:.1}", change),
    ))
}

/// Caption for the chart, labelled by range (e.g. "All-time +2.3¢ · range 8.0–22.0¢").
/// `range_label` is "24h" | "1W" | "All-time". Falls back gracefully on thin data.
pub fn chart_caption(points: &[(i64, Decimal)], range_label: &str) -> String {
    match history_range(points) {
        Some((lo, hi, chg)) => format!("{range_label} {chg}\u{00a2} \u{00b7} range {lo}\u{2013}{hi}\u{00a2}"),
        None => format!("{range_label} \u{00b7} not enough history yet"),
    }
}

// ── Price chart (server-rendered inline SVG — no JS/dep) ──────────────────────

/// Build a clean sparkline of the price history: a soft gradient area under a
/// crisp price line, auto-scaled to the data with a little vertical breathing
/// room, plus a faint dotted guide at the latest price. Returns `<svg>…</svg>`
/// markup (render with `|safe`). The reward band is shown on the order-book
/// ladder (where placement happens), not here — keeping the chart uncluttered.
pub fn price_chart_svg(points: &[(i64, Decimal)]) -> String {
    const W: f64 = 600.0;
    const H: f64 = 220.0;
    const PAD_Y: f64 = 16.0;
    if points.len() < 2 {
        return format!(
            "<svg viewBox=\"0 0 {W} {H}\" class=\"chart-svg\" preserveAspectRatio=\"none\">\
             <text x=\"{}\" y=\"{}\" class=\"chart-empty\" text-anchor=\"middle\">Not enough price history yet</text></svg>",
            W / 2.0, H / 2.0
        );
    }
    // Cap to ~120 points (stride-downsample, always keeping the last point) so a
    // wide range like "Max" stays a small, smooth SVG regardless of API density.
    const MAX_POINTS: usize = 120;
    let to_f = |d: Decimal| d.to_string().parse::<f64>().unwrap_or(0.0);
    let prices: Vec<f64> = if points.len() > MAX_POINTS {
        let stride = points.len().div_ceil(MAX_POINTS);
        let last = points.len() - 1;
        points.iter().enumerate()
            .filter(|(i, _)| i % stride == 0 || *i == last)
            .map(|(_, (_, p))| to_f(*p))
            .collect()
    } else {
        points.iter().map(|(_, p)| to_f(*p)).collect()
    };
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &p in &prices { lo = lo.min(p); hi = hi.max(p); }
    if (hi - lo).abs() < f64::EPSILON { lo -= 0.01; hi += 0.01; }
    // Pad the value range ~10% each side so the line never hugs the edges.
    let pad = (hi - lo) * 0.10;
    lo -= pad; hi += pad;
    let span = hi - lo;

    // NB: plot over the (possibly downsampled) `prices`, so `n` must be its
    // length — using the original points.len() squished the line into the left
    // edge whenever the history was downsampled (>120 pts).
    let n = prices.len() as f64;
    let x = |i: usize| (i as f64) / (n - 1.0) * W;
    let y = |p: f64| PAD_Y + (1.0 - (p - lo) / span) * (H - 2.0 * PAD_Y);

    let line: String = prices.iter().enumerate()
        .map(|(i, &p)| format!("{}{:.1},{:.1}", if i == 0 { "M" } else { "L" }, x(i), y(p)))
        .collect::<Vec<_>>()
        .join(" ");
    // Close the area down to the baseline and back to the start.
    let last_i = prices.len() - 1;
    let area = format!("{line} L{:.1},{H:.1} L0.0,{H:.1} Z", x(last_i));
    let last_y = y(prices[last_i]);
    // Unique gradient id per SVG (several charts share the page → no dup ids).
    let gid = format!("pf-area-{:x}", points.first().map(|(t, _)| *t).unwrap_or(0) as u64 ^ (prices.len() as u64));

    format!(
        "<svg viewBox=\"0 0 {W} {H}\" class=\"chart-svg\" preserveAspectRatio=\"none\">\
         <defs><linearGradient id=\"{gid}\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\
         <stop offset=\"0\" stop-color=\"rgb(108,140,255)\" stop-opacity=\"0.22\"/>\
         <stop offset=\"1\" stop-color=\"rgb(108,140,255)\" stop-opacity=\"0\"/></linearGradient></defs>\
         <path d=\"{area}\" fill=\"url(#{gid})\"/>\
         <line x1=\"0\" y1=\"{last_y:.1}\" x2=\"{W}\" y2=\"{last_y:.1}\" class=\"chart-guide\" vector-effect=\"non-scaling-stroke\"/>\
         <path d=\"{line}\" class=\"chart-line\" fill=\"none\" vector-effect=\"non-scaling-stroke\"/></svg>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(best_bid: Decimal, levels: &[(Decimal, Decimal)]) -> BookSnapshot {
        BookSnapshot {
            bids: levels.to_vec(),
            asks: vec![],
            best_bid: Some(best_bid),
            best_ask: None,
            tick_size: dec!(0.01),
            min_order_size: dec!(20),
        }
    }

    #[test]
    fn qualifies_within_band_and_min() {
        let b = book(dec!(0.40), &[(dec!(0.40), dec!(1000))]);
        // midpoint 0.42, max spread 3c → band [0.39, 0.42). Place at 0.40 (2c off).
        let e = evaluate_placement(&b, dec!(0.42), dec!(3), dec!(20), dec!(0.40), dec!(50), false);
        assert!(e.qualifies);
        assert_eq!(e.cents_from_mid, dec!(2.00));
        assert!(e.per_side_meets_min);
    }

    #[test]
    fn does_not_qualify_outside_band() {
        let b = book(dec!(0.40), &[(dec!(0.40), dec!(1000))]);
        // 5c below midpoint, band only 3c → out.
        let e = evaluate_placement(&b, dec!(0.42), dec!(3), dec!(20), dec!(0.37), dec!(50), false);
        assert!(!e.qualifies);
    }

    #[test]
    fn both_sides_halves_size_and_can_break_min() {
        let b = book(dec!(0.40), &[(dec!(0.40), dec!(1000))]);
        // Min is a SHARE count (20). $12 split → $6/side; at 0.40 that's only
        // 15 shares/side < 20 → does not qualify even though it's in band.
        let e = evaluate_placement(&b, dec!(0.42), dec!(3), dec!(20), dec!(0.40), dec!(12), true);
        assert_eq!(e.per_side_size, dec!(6));
        assert_eq!(e.per_side_shares, dec!(15));
        assert!(!e.per_side_meets_min);
        assert!(!e.qualifies);
    }

    #[test]
    fn fill_risk_high_at_or_above_best_bid() {
        let b = book(dec!(0.40), &[(dec!(0.40), dec!(1000))]);
        let e = evaluate_placement(&b, dec!(0.42), dec!(5), dec!(20), dec!(0.40), dec!(50), false);
        assert_eq!(e.fill_risk, FillRisk::High);
    }

    #[test]
    fn fill_risk_low_with_depth_ahead() {
        // Lots of depth between our 0.38 and best bid 0.40 → low risk.
        let b = book(dec!(0.40), &[(dec!(0.40), dec!(1000)), (dec!(0.39), dec!(1000))]);
        let e = evaluate_placement(&b, dec!(0.42), dec!(5), dec!(20), dec!(0.38), dec!(50), false);
        assert_eq!(e.fill_risk, FillRisk::Low);
    }

    #[test]
    fn score_weight_is_quadratic_and_zero_at_band_edge() {
        // v = 4c. At the midpoint → 1; 2c away → (2/4)² = 0.25; at/after edge → 0.
        assert_eq!(score_weight(dec!(0.50), dec!(0.50), dec!(4)), dec!(1));
        assert_eq!(score_weight(dec!(0.50), dec!(0.48), dec!(4)), dec!(0.25));
        assert_eq!(score_weight(dec!(0.50), dec!(0.46), dec!(4)), dec!(0));
        assert_eq!(score_weight(dec!(0.50), dec!(0.40), dec!(4)), dec!(0));
        // No reward program → 0.
        assert_eq!(score_weight(dec!(0.50), dec!(0.49), dec!(0)), dec!(0));
    }

    #[test]
    fn one_sided_is_divided_by_three_in_range_and_zero_outside() {
        let w = dec!(0.9);
        // Midpoint inside [0.10, 0.90]: one-sided scores w / 3.
        assert_eq!(effective_weight(dec!(0.50), w, None), dec!(0.3));
        // Outside: one-sided scores nothing.
        assert_eq!(effective_weight(dec!(0.05), w, None), dec!(0));
        assert_eq!(effective_weight(dec!(0.95), w, None), dec!(0));
        // Two-sided outside the range: the weaker leg counts.
        assert_eq!(effective_weight(dec!(0.05), w, Some(dec!(0.4))), dec!(0.4));
        // Two-sided inside: max(min(a,b), max(a,b)/3).
        assert_eq!(effective_weight(dec!(0.50), w, Some(dec!(0.6))), dec!(0.6));
        assert_eq!(effective_weight(dec!(0.50), w, Some(dec!(0.1))), dec!(0.3));
    }

    #[test]
    fn presets_hit_their_target_weight_and_stay_below_best_bid() {
        // mid 0.50, v = 10c, tick 0.001: plenty of resolution.
        let ps = placement_presets(dec!(0.50), dec!(10), dec!(0.001), Some(dec!(0.499)));
        let keys: Vec<_> = ps.iter().map(|p| p.key).collect();
        assert_eq!(keys, vec!["safer", "balanced", "tight"]);
        for (p, target) in ps.iter().zip([dec!(0.25), dec!(0.5), dec!(0.75)]) {
            assert!(p.weight >= target, "{} weight {} < {}", p.key, p.weight, target);
            assert!(p.price < dec!(0.499));
        }
        // Safer: s ≤ 5c → 0.450, weight exactly 0.25.
        assert_eq!(ps[0].price, dec!(0.45));
        assert_eq!(ps[0].weight, dec!(0.25));
    }

    #[test]
    fn presets_never_suggest_a_zero_score_price() {
        // Coarse tick vs. a tight band: presets may collapse but never score 0.
        let ps = placement_presets(dec!(0.505), dec!(3), dec!(0.01), Some(dec!(0.50)));
        assert!(!ps.is_empty());
        assert!(ps.iter().all(|p| p.weight > dec!(0) && p.price < dec!(0.50)));
        // No rewards → no presets.
        assert!(placement_presets(dec!(0.50), dec!(0), dec!(0.01), None).is_empty());
    }

    #[test]
    fn ladder_marks_your_live_orders() {
        let mut b = book(dec!(0.40), &[(dec!(0.40), dec!(100)), (dec!(0.38), dec!(50))]);
        b.asks = vec![(dec!(0.42), dec!(10))];
        b.best_ask = Some(dec!(0.42));
        let l = build_ladder(&b, dec!(0.41), dec!(3), 10, &[dec!(0.38)], dec!(0.01));
        let marked: Vec<_> = l.bids.iter().filter(|r| r.is_live_mine).map(|r| r.price_raw.clone()).collect();
        assert_eq!(marked, vec!["0.38".to_string()]);
        assert!(l.asks.iter().all(|r| !r.is_live_mine));
    }

    #[test]
    fn ladder_shows_cumulative_usd_depth() {
        // bids: 100 @ 0.40 = $40, then 50 @ 0.38 = $19 → cumulative $40, $59.
        let b = book(dec!(0.40), &[(dec!(0.40), dec!(100)), (dec!(0.38), dec!(50))]);
        let l = build_ladder(&b, dec!(0.41), dec!(3), 10, &[], dec!(0.01));
        assert_eq!(l.bids[0].total_usd, "$40");
        assert_eq!(l.bids[1].total_usd, "$59");
        assert_eq!(l.bids[1].total, "150");
    }
}
