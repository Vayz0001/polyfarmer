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
pub async fn fetch_history(token_id: &str, interval: Interval) -> Result<Vec<(i64, Decimal)>> {
    let req = PriceHistoryRequest::builder()
        .market(token_u256(token_id)?)
        .time_range(interval)
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
    /// Size that actually rests on this side ( = order_size, or /2 if both_sides).
    pub per_side_size: Decimal,
    pub per_side_meets_min: bool,
    pub fill_risk: FillRisk,
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
    let per_side_meets_min = per_side_size >= min_size;

    // Reward-qualifying: within max_spread of the midpoint (we farm the bid
    // side, so the order sits at or below the midpoint) AND each resting order
    // meets the minimum size.
    let within_band = cents_from_mid >= dec!(0) && cents_from_mid <= max_spread_cents;
    let qualifies = within_band && per_side_meets_min;

    PlacementEval {
        qualifies,
        cents_from_mid,
        per_side_size,
        per_side_meets_min,
        fill_risk: fill_risk(book, your_price, per_side_size),
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

/// A conservative starting placement: the far edge of the reward band
/// (`midpoint − max_spread`), snapped down to a tick. That's the lowest-fill-
/// risk price that still qualifies — exactly where a cautious farmer parks.
/// Returns `None` if the band is degenerate (e.g. no rewards configured).
pub fn suggest_placement(
    midpoint: Decimal,
    max_spread_cents: Decimal,
    tick: Decimal,
) -> Option<Decimal> {
    if max_spread_cents <= dec!(0) {
        return None;
    }
    let edge = midpoint - max_spread_cents / dec!(100);
    let snapped = TokenBook::snap_to_tick(edge, tick);
    if snapped <= dec!(0) || snapped >= midpoint {
        return None;
    }
    Some(snapped)
}

// ── Order-book ladder view-model (presentation) ──────────────────────────────

pub struct LadderRow {
    pub price_cents: String,  // "34.0"
    pub price_raw: String,    // "0.34" — the value click-to-set writes
    pub size: String,         // shares at this level, compact
    pub total: String,        // cumulative shares from the best level, compact
    pub depth_pct: u32,       // 0-100, cumulative-depth bar width
    pub side: &'static str,   // "ask" | "bid"
    pub in_band: bool,        // within the reward-qualifying zone
    pub is_best: bool,
    pub is_yours: bool,       // this level == your current placement price
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
    pub has_book: bool,
}

/// Build the display ladder: top `per_side` levels each side, cumulative-depth
/// bars (shares, normalized across shown levels), reward band shaded. Reward band
/// = within `max_spread_cents` of the midpoint on either side. `your_price`, when
/// set, flags the level you'd rest at so the book shows a "you are here" marker.
/// `group` is the price-bucket size for the grouping control (== tick → raw).
pub fn build_ladder(
    book: &BookSnapshot,
    midpoint: Decimal,
    max_spread_cents: Decimal,
    per_side: usize,
    your_price: Option<Decimal>,
    group: Decimal,
) -> Ladder {
    let band = max_spread_cents / dec!(100);
    let band_lo = midpoint - band;
    let band_hi = midpoint + band;
    let yours = your_price.map(|p| p.normalize());

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
    let your_bid = yours.map(|y| if group > dec!(0) { ((y / group).floor() * group).normalize() } else { y });
    let your_ask = yours.map(|y| if group > dec!(0) { ((y / group).ceil() * group).normalize() } else { y });

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
    let max_level = bid_levels.iter().chain(ask_levels.iter())
        .map(|(_, s)| *s).fold(dec!(0), Decimal::max);

    let pct = |s: Decimal| -> u32 {
        if max_level <= dec!(0) { 0 } else { ((s / max_level) * dec!(100)).round().to_u32().unwrap_or(0).min(100) }
    };
    let row = |p: Decimal, s: Decimal, cumv: Decimal, side: &'static str, is_best: bool| LadderRow {
        price_cents: format!("{:.1}", p * dec!(100)),
        price_raw: p.normalize().to_string(),
        size: fmt_shares(s),
        total: fmt_shares(cumv),
        depth_pct: pct(s),
        side,
        in_band: p > band_lo && p < band_hi,
        is_best,
        is_yours: if side == "bid" { your_bid == Some(p.normalize()) } else { your_ask == Some(p.normalize()) },
    };

    // asks displayed high→low so the best ask sits just above the midpoint line.
    let asks: Vec<LadderRow> = ask_levels.iter().zip(ask_cum.iter()).enumerate()
        .map(|(i, ((p, s), c))| row(*p, *s, *c, "ask", i == 0))
        .rev()
        .collect();
    let bids: Vec<LadderRow> = bid_levels.iter().zip(bid_cum.iter()).enumerate()
        .map(|(i, ((p, s), c))| row(*p, *s, *c, "bid", i == 0))
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
         <stop offset=\"0\" stop-color=\"rgba(255,77,141,0.18)\"/>\
         <stop offset=\"1\" stop-color=\"rgba(255,77,141,0)\"/></linearGradient></defs>\
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
        // $30 split → $15/side < $20 min → does not qualify even though in band.
        let e = evaluate_placement(&b, dec!(0.42), dec!(3), dec!(20), dec!(0.40), dec!(30), true);
        assert_eq!(e.per_side_size, dec!(15));
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
    fn suggest_is_far_edge_snapped() {
        // midpoint 0.50, max spread 3c → edge 0.47, tick 0.01 → 0.47.
        assert_eq!(suggest_placement(dec!(0.50), dec!(3), dec!(0.01)), Some(dec!(0.47)));
        // no rewards → none.
        assert_eq!(suggest_placement(dec!(0.50), dec!(0), dec!(0.01)), None);
    }
}
