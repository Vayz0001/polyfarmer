//! Resolve a pasted Polymarket URL into the fields needed to build a
//! `MarketConfig` — via the SDK's typed Gamma client (no hand-rolled JSON
//! parsing, unlike the old TS bot: `clob_token_ids`/`outcomes` already come
//! back as real arrays, not JSON-encoded strings).
//!
//! Deliberately does NOT touch `Executor` — Gamma is public/unauthenticated,
//! and a market must be addable even before the engine has started (the web
//! handler just won't get a live executor to subscribe with yet).

use chrono::{DateTime, Utc};
use eyre::Result;
use polymarket_client_sdk_v2::gamma::types::request::{EventBySlugRequest, MarketBySlugRequest};
use polymarket_client_sdk_v2::gamma::types::response::Market;
use polymarket_client_sdk_v2::gamma::Client as GammaClient;
use rust_decimal::Decimal;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::cache::TtlCache;

// ── New resolution model (binary-market-aware, outcome-agnostic) ─────────────
// Every tradeable CLOB market is binary (exactly 2 outcome tokens); a
// multi-outcome *event* (e.g. "World Cup Winner") is N separate binary
// sub-markets. So resolving a URL yields either one market or a candidate list.

/// One binary market, fully resolved — both outcome tokens, reward params,
/// tick. Outcome labels are whatever the market uses (`Yes`/`No`, team names,
/// `Up`/`Down`, …) — never assume.
#[derive(Clone)]
pub struct MarketRef {
    pub condition_id: String,
    pub question: String,
    /// Candidate name within a multi-market event (e.g. "France"), if any.
    pub group_item_title: Option<String>,
    pub market_slug: String,
    /// Parent event's slug, when the market belongs to one. Polymarket URLs for
    /// a grouped market are `/event/{event_slug}/{market_slug}`; standalone
    /// binaries are just `/event/{market_slug}` (event_slug == market_slug).
    pub event_slug: String,
    pub image: Option<String>,
    pub outcomes: Vec<String>,      // exactly 2
    pub token_ids: Vec<String>,     // exactly 2, aligned with `outcomes`
    pub outcome_prices: Vec<Decimal>,
    pub tick_size: Decimal,
    pub rewards_min_size: Option<Decimal>,
    pub rewards_max_spread: Option<Decimal>,
    pub volume_24hr: Option<Decimal>,
    pub liquidity: Option<Decimal>,
    pub last_trade_price: Option<Decimal>,
    pub one_day_price_change: Option<Decimal>,
    pub end_date: Option<DateTime<Utc>>,
}

impl MarketRef {
    fn from_market(m: Market) -> Result<Self> {
        let event_slug = m
            .events
            .as_ref()
            .and_then(|evs| evs.first())
            .and_then(|ev| ev.slug.clone())
            .unwrap_or_default();
        let outcomes = m.outcomes.unwrap_or_default();
        let token_ids: Vec<String> =
            m.clob_token_ids.unwrap_or_default().iter().map(|t| t.to_string()).collect();
        if outcomes.len() != 2 || token_ids.len() != 2 {
            eyre::bail!("market is not a tradeable binary market (outcomes/tokens != 2)");
        }
        let condition_id = m
            .condition_id
            .ok_or_else(|| eyre::eyre!("market has no condition id (not tradeable yet?)"))?
            .to_string();
        let tick_size = m
            .order_price_min_tick_size
            .ok_or_else(|| eyre::eyre!("market has no tick size (not tradeable yet?)"))?;
        Ok(Self {
            condition_id,
            question: m.question.unwrap_or_default(),
            group_item_title: m.group_item_title,
            market_slug: m.slug.unwrap_or_default(),
            event_slug,
            image: m.image,
            outcome_prices: m.outcome_prices.unwrap_or_default(),
            outcomes,
            token_ids,
            tick_size,
            rewards_min_size: m.rewards_min_size,
            rewards_max_spread: m.rewards_max_spread,
            volume_24hr: m.volume_24hr,
            liquidity: m.liquidity,
            last_trade_price: m.last_trade_price,
            one_day_price_change: m.one_day_price_change,
            end_date: m.end_date,
        })
    }

    /// True once Polymarket has a live reward program on this market.
    pub fn has_rewards(&self) -> bool {
        self.rewards_min_size.is_some_and(|s| s > Decimal::ZERO)
            && self.rewards_max_spread.is_some_and(|s| s > Decimal::ZERO)
    }
}

/// Resolution outcome for a pasted URL: one market → go straight to the
/// detail view; many → show a picker.
pub enum Resolved {
    Single(Box<MarketRef>),
    Multiple(Vec<MarketRef>),
}

/// One shared Gamma client (connection reuse) for every lookup.
static GAMMA: LazyLock<GammaClient> = LazyLock::new(GammaClient::default);

/// Market metadata barely changes (question, tokens, tick, reward params), but
/// the market view, its live book, the placement preview and the markets table
/// all need it — cache it so none of them re-hit Gamma on every request.
const MARKET_TTL: Duration = Duration::from_secs(60);
static MARKETS: LazyLock<Arc<TtlCache<MarketRef>>> =
    LazyLock::new(|| Arc::new(TtlCache::new(MARKET_TTL)));

/// Load one binary market by its market slug — always a fresh Gamma call.
pub async fn market_by_slug(slug: &str) -> Result<MarketRef> {
    let m = GAMMA
        .market_by_slug(&MarketBySlugRequest::builder().slug(slug.to_string()).build())
        .await?;
    MarketRef::from_market(m)
}

/// [`market_by_slug`] through the shared TTL cache (stale value on upstream error).
pub async fn market_by_slug_cached(slug: &str) -> Result<MarketRef> {
    MARKETS.get_or_fetch(slug, || market_by_slug(slug)).await
}

/// Non-blocking cache read for render paths that must not wait on Gamma (the
/// markets table): returns what's cached now and refreshes in the background.
pub fn market_by_slug_swr(slug: &str) -> Option<MarketRef> {
    let owned = slug.to_string();
    MARKETS.get_swr(slug, move || async move { market_by_slug(&owned).await })
}

/// Resolve a pasted Polymarket URL. A market-slug URL → `Single`; an
/// event-slug URL → `Single` if the event has one tradeable market, else
/// `Multiple` (the candidate list for the picker).
pub async fn resolve_url(url: &str) -> Result<Resolved> {
    let slug = extract_slug(url)?;
    let gamma = &*GAMMA;

    if let Ok(m) = gamma
        .market_by_slug(&MarketBySlugRequest::builder().slug(slug.clone()).build())
        .await
    {
        if let Ok(mr) = MarketRef::from_market(m) {
            return Ok(Resolved::Single(Box::new(mr)));
        }
    }

    // Fall back to treating the slug as an event (multi-market).
    let event = gamma
        .event_by_slug(&EventBySlugRequest::builder().slug(slug).build())
        .await?;
    let mut refs: Vec<MarketRef> = event
        .markets
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.enable_order_book.unwrap_or(false) && !m.closed.unwrap_or(false))
        .filter_map(|m| MarketRef::from_market(m).ok())
        .collect();
    match refs.len() {
        0 => eyre::bail!("No tradeable markets found for this URL"),
        1 => Ok(Resolved::Single(Box::new(refs.pop().unwrap()))),
        _ => Ok(Resolved::Multiple(refs)),
    }
}

/// Pulls the rightmost path segment after `/event/` — handles both
/// `polymarket.com/event/<slug>` and `polymarket.com/event/<event-slug>/<market-slug>`.
fn extract_slug(url: &str) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let after = trimmed
        .split("/event/")
        .nth(1)
        .ok_or_else(|| eyre::eyre!("Not a Polymarket market URL (expected .../event/<slug>)"))?;
    let slug = after.rsplit('/').next().unwrap_or(after);
    if slug.is_empty() {
        eyre::bail!("Could not find a market slug in the URL");
    }
    Ok(slug.to_string())
}
