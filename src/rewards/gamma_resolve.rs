//! Resolve a pasted Polymarket URL into the fields needed to build a
//! `MarketConfig` — via the SDK's typed Gamma client (no hand-rolled JSON
//! parsing: `clob_token_ids`/`outcomes` already come back as real arrays, not
//! JSON-encoded strings).
//!
//! Deliberately does NOT touch `Executor` — Gamma is public/unauthenticated,
//! and a market must be addable even before the engine has started (the web
//! handler just won't get a live executor to subscribe with yet).

use chrono::{DateTime, NaiveDate, Utc};
use eyre::Result;
use polymarket_client_sdk_v2::gamma::types::request::{EventBySlugRequest, MarketBySlugRequest};
use polymarket_client_sdk_v2::gamma::types::response::{ClobReward, Market};
use polymarket_client_sdk_v2::gamma::Client as GammaClient;
use reqwest::Url;
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
    pub outcomes: Vec<String>,  // exactly 2
    pub token_ids: Vec<String>, // exactly 2, aligned with `outcomes`
    pub outcome_prices: Vec<Decimal>,
    pub tick_size: Decimal,
    pub rewards_min_size: Option<Decimal>,
    pub rewards_max_spread: Option<Decimal>,
    /// USDC Polymarket pays per day across this market's active reward
    /// programs. Zero when there is none: most markets carry a min size and
    /// zone (often 50 shares / 4.5¢) without any pool behind them.
    pub reward_pool: Decimal,
    pub volume_24hr: Option<Decimal>,
    pub liquidity: Option<Decimal>,
    pub last_trade_price: Option<Decimal>,
    pub one_day_price_change: Option<Decimal>,
    pub end_date: Option<DateTime<Utc>>,
}

impl MarketRef {
    fn from_market(m: Market) -> Result<Self> {
        let event_slug =
            m.events.as_ref().and_then(|evs| evs.first()).and_then(|ev| ev.slug.clone()).unwrap_or_default();
        let outcomes = m.outcomes.unwrap_or_default();
        let token_ids: Vec<String> = m.clob_token_ids.unwrap_or_default().iter().map(|t| t.to_string()).collect();
        if outcomes.len() != 2 || token_ids.len() != 2 {
            eyre::bail!("market is not a tradeable binary market (outcomes/tokens != 2)");
        }
        let condition_id =
            m.condition_id.ok_or_else(|| eyre::eyre!("market has no condition id (not tradeable yet?)"))?.to_string();
        let tick_size =
            m.order_price_min_tick_size.ok_or_else(|| eyre::eyre!("market has no tick size (not tradeable yet?)"))?;
        let reward_pool = daily_pool(m.clob_rewards.as_deref().unwrap_or_default(), Utc::now().date_naive());
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
            reward_pool,
            volume_24hr: m.volume_24hr,
            liquidity: m.liquidity,
            last_trade_price: m.last_trade_price,
            one_day_price_change: m.one_day_price_change,
            end_date: m.end_date,
        })
    }

    /// True once Polymarket has a live reward program on this market: a pool
    /// that pays today, plus the min size and zone that decide who earns it.
    pub fn has_rewards(&self) -> bool {
        self.reward_pool > Decimal::ZERO
            && self.rewards_min_size.is_some_and(|s| s > Decimal::ZERO)
            && self.rewards_max_spread.is_some_and(|s| s > Decimal::ZERO)
    }
}

/// Sum of the daily rates of the reward programs running on `today` (UTC).
/// A missing start or end date counts as open-ended.
fn daily_pool(programs: &[ClobReward], today: NaiveDate) -> Decimal {
    programs
        .iter()
        .filter(|p| p.start_date.is_none_or(|d| d <= today) && p.end_date.is_none_or(|d| d >= today))
        .filter_map(|p| p.rewards_daily_rate)
        .filter(|r| *r > Decimal::ZERO)
        .sum()
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
static MARKETS: LazyLock<Arc<TtlCache<MarketRef>>> = LazyLock::new(|| Arc::new(TtlCache::new(MARKET_TTL)));

/// Load one binary market by its market slug — always a fresh Gamma call.
pub async fn market_by_slug(slug: &str) -> Result<MarketRef> {
    let m = GAMMA.market_by_slug(&MarketBySlugRequest::builder().slug(slug.to_string()).build()).await?;
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

/// Resolve a pasted Polymarket link. A link to one market → `Single`. A page
/// for a whole event or game → `Single` if it has one tradeable market, else
/// `Multiple` (the picker's candidates, reward-paying markets first).
pub async fn resolve_url(url: &str) -> Result<Resolved> {
    match parse_link(url)? {
        Link::Market(slug) => market_by_slug(&slug)
            .await
            .ok()
            .filter(|mr| valid_slug(&mr.market_slug))
            .map(|mr| Resolved::Single(Box::new(mr)))
            .ok_or_else(|| eyre::eyre!("Polymarket has no tradeable market called \"{slug}\"")),
        Link::Event(slug) => resolve_event(&slug).await,
    }
}

async fn resolve_event(slug: &str) -> Result<Resolved> {
    let event = match GAMMA.event_by_slug(&EventBySlugRequest::builder().slug(slug.to_string()).build()).await {
        Ok(e) => e,
        // Older links put a standalone market's own slug after /event/.
        Err(event_err) => {
            let standalone = market_by_slug(slug).await.ok().filter(|mr| valid_slug(&mr.market_slug));
            return standalone.map(|mr| Resolved::Single(Box::new(mr))).ok_or_else(|| {
                if event_err.to_string().contains("404") {
                    eyre::eyre!(
                        "Polymarket has no market or event called \"{slug}\". \
                         For sports, open a single game and paste its link."
                    )
                } else {
                    eyre::eyre!("Could not load \"{slug}\" from Polymarket: {event_err}")
                }
            });
        }
    };
    let mut refs: Vec<MarketRef> = event
        .markets
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.enable_order_book.unwrap_or(false) && !m.closed.unwrap_or(false))
        .filter_map(|m| MarketRef::from_market(m).ok())
        // The slug goes into our own links (redirect, picker hrefs): keep
        // only the ordinary ones Polymarket actually uses.
        .filter(|mr| valid_slug(&mr.market_slug))
        .collect();
    sort_candidates(&mut refs, slug);
    match refs.len() {
        0 => Err(eyre::eyre!("No open markets on this page")),
        1 => Ok(Resolved::Single(Box::new(refs.pop().unwrap()))),
        _ => Ok(Resolved::Multiple(refs)),
    }
}

/// Picker order: markets that pay rewards first, the page's main market (the
/// one sharing the event's slug, e.g. a game's match winner) at the top, then
/// the largest daily pool. Everything else keeps Polymarket's own order.
fn sort_candidates(refs: &mut [MarketRef], event_slug: &str) {
    refs.sort_by(|a, b| {
        b.has_rewards()
            .cmp(&a.has_rewards())
            .then_with(|| (b.market_slug == event_slug).cmp(&(a.market_slug == event_slug)))
            .then_with(|| b.reward_pool.cmp(&a.reward_pool))
    });
}

/// What a pasted link points at.
#[derive(Debug, PartialEq, Eq)]
enum Link {
    /// A page for a whole event or game: one market or many.
    Event(String),
    /// One specific market.
    Market(String),
}

/// Polymarket slugs are lowercase words joined by dashes; this is looser on
/// purpose but still keeps anything path- or query-like out of API requests.
const MAX_SLUG_LEN: usize = 200;

pub fn valid_slug(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_SLUG_LEN && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A language prefix such as `es` or `pt-br` in `polymarket.com/es/event/…`.
fn is_locale(seg: &str) -> bool {
    let b = seg.as_bytes();
    let letters = |s: &[u8]| s.iter().all(u8::is_ascii_alphabetic);
    match b.len() {
        2 => letters(b),
        5 => b[2] == b'-' && letters(&b[..2]) && letters(&b[3..]),
        _ => false,
    }
}

/// Read a polymarket.com link (query string and fragment already ignored):
/// - `/event/<event>` and the game pages `/sports/<league>/<game>` and
///   `/esports/<game>/<tournament>/<match>` → the whole event;
/// - `/event/<event>/<market>` and `/market/<market>` → that one market.
fn parse_link(raw: &str) -> Result<Link> {
    const HINT: &str = "Paste the link of a Polymarket market, event or game page";
    let url = Url::parse(raw.trim()).map_err(|_| eyre::eyre!("{HINT}"))?;
    if !matches!(url.scheme(), "https" | "http")
        || !matches!(url.host_str(), Some("polymarket.com" | "www.polymarket.com"))
    {
        eyre::bail!("{HINT} (polymarket.com)");
    }
    let mut segs: Vec<&str> = url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    if segs.len() > 1 && is_locale(segs[0]) {
        segs.remove(0);
    }
    let link = match segs.as_slice() {
        ["event", e] => Link::Event(e.to_string()),
        ["event", _, m] | ["market", m] => Link::Market(m.to_string()),
        // `/sports/nba/<game>`, older `/sports/nba/games/week/1/<game>`,
        // `/esports/cs2/<tournament>/<match>`: the game is always last.
        ["sports" | "esports", _, .., game] => Link::Event(game.to_string()),
        _ => return Err(eyre::eyre!("{HINT}")),
    };
    let (Link::Event(slug) | Link::Market(slug)) = &link;
    if !valid_slug(slug) {
        eyre::bail!("{HINT}");
    }
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn ev(s: &str) -> Link {
        Link::Event(s.into())
    }
    fn mk(s: &str) -> Link {
        Link::Market(s.into())
    }

    #[test]
    fn reads_every_polymarket_link_shape() {
        let cases = [
            // The link from the bug report (query and fragment are stripped
            // earlier by the web handler; Url ignores them anyway).
            (
                "https://polymarket.com/esports/league-of-legends/worlds/lol-blg-gen-2026-11-14",
                ev("lol-blg-gen-2026-11-14"),
            ),
            (
                "https://polymarket.com/esports/league-of-legends/worlds/lol-blg-gen-2026-11-14?r=earnfees#L97Nndk",
                ev("lol-blg-gen-2026-11-14"),
            ),
            ("https://polymarket.com/esports/cs2/blast-rivals/cs2-ts7-mouz-2026-11-14", ev("cs2-ts7-mouz-2026-11-14")),
            ("https://polymarket.com/sports/nba/nba-chi-min-2026-11-14", ev("nba-chi-min-2026-11-14")),
            ("https://polymarket.com/sports/nba/games/week/1/nba-chi-min-2026-11-14", ev("nba-chi-min-2026-11-14")),
            ("https://polymarket.com/sports/epl/epl-che-lee-2026-11-21/", ev("epl-che-lee-2026-11-21")),
            ("https://polymarket.com/event/fed-decision-in-october", ev("fed-decision-in-october")),
            ("https://www.polymarket.com/event/fed-decision-in-october", ev("fed-decision-in-october")),
            ("http://POLYMARKET.com/event/fed-decision-in-october", ev("fed-decision-in-october")),
            ("https://polymarket.com:443/event/fed-decision-in-october", ev("fed-decision-in-october")),
            ("https://polymarket.com/es/event/fed-decision-in-october", ev("fed-decision-in-october")),
            ("https://polymarket.com/pt-br/event/fed-decision-in-october", ev("fed-decision-in-october")),
            (
                "https://polymarket.com/event/lol-blg-gen-2026-11-14/lol-blg-gen-2026-11-14-game1",
                mk("lol-blg-gen-2026-11-14-game1"),
            ),
            ("https://polymarket.com/market/lol-blg-gen-2026-11-14-game1", mk("lol-blg-gen-2026-11-14-game1")),
        ];
        for (url, want) in cases {
            assert_eq!(parse_link(url).unwrap(), want, "{url}");
        }
    }

    #[test]
    fn rejects_links_that_are_not_a_market_event_or_game() {
        for url in [
            "",
            "not a link",
            "https://evil.com/event/fed-decision-in-october",
            "https://polymarket.com.evil.com/event/x",
            "https://polymarket.com@evil.com/event/x",
            "https://notpolymarket.com/event/x",
            "ftp://polymarket.com/event/x",
            "https://polymarket.com/",
            "https://polymarket.com/event",
            "https://polymarket.com/sports/nba",
            "https://polymarket.com/predictions/politics/extra/x",
            "https://polymarket.com/event/a/b/c",
            "https://polymarket.com/profile/0xabc",
            "https://polymarket.com/es",
            // Anything path- or query-like never reaches a Gamma request.
            "https://polymarket.com/event/..",
            "https://polymarket.com/event/a%2Fb",
            "https://polymarket.com/event/a%3Fb",
            "https://polymarket.com/event/a.b",
            "https://polymarket.com/market/%2e%2e",
        ] {
            assert!(parse_link(url).is_err(), "{url}");
        }
        let long = format!("https://polymarket.com/event/{}", "a".repeat(MAX_SLUG_LEN + 1));
        assert!(parse_link(&long).is_err());
        let max = format!("https://polymarket.com/event/{}", "a".repeat(MAX_SLUG_LEN));
        assert!(parse_link(&max).is_ok());
    }

    /// Thousands of generated inputs (hosts, schemes, encodings, unicode,
    /// control characters, odd separators): parsing never panics, and every
    /// slug it lets through is a plain one.
    #[test]
    fn junk_links_never_panic_or_leak_odd_slugs() {
        let schemes = ["", "https://", "http://", "HTTPS://", "javascript:", "file://", "//", "https:/", "ftp://"];
        let hosts = [
            "polymarket.com",
            "www.polymarket.com",
            "POLYMARKET.COM",
            "polymarket.com:443",
            "user@polymarket.com",
            "polymarket.com@evil.com",
            "evil.com",
            "polymarket.com.evil.com",
            "[::1]",
            "xn--polymarket-.com",
            "",
        ];
        let paths = [
            "",
            "/",
            "/event",
            "/event/",
            "/event/a",
            "/event/a/b",
            "/event/a/b/c",
            "/market/a",
            "/sports/nba/a",
            "/esports/x/y/a",
            "/es/event/a",
            "/zz-zz/market/a",
            "/event/%2e%2e",
            "/event/a%2fb",
            "/event/a\\b",
            "/event/é",
            "/event/a b",
            "/event/\u{0}",
            "/event/a?b=c#d",
            "/event//a",
            "/./event/a",
            "/event/a/../b",
            "/sports/../event/a",
            "/event/a;b",
            "/event/a%00",
            "/event/🙂",
        ];
        let mut n = 0;
        for s in schemes {
            for h in hosts {
                for p in paths {
                    let raw = format!("{s}{h}{p}");
                    if let Ok(Link::Event(slug) | Link::Market(slug)) = parse_link(&raw) {
                        assert!(valid_slug(&slug), "{raw:?} → {slug:?}");
                        assert!(
                            matches!(
                                h,
                                "polymarket.com"
                                    | "www.polymarket.com"
                                    | "POLYMARKET.COM"
                                    | "polymarket.com:443"
                                    | "user@polymarket.com"
                            ),
                            "{raw:?}"
                        );
                    }
                    n += 1;
                }
            }
        }
        assert!(n > 2000);
        // Pseudo-random bytes (fixed seed, so failures reproduce).
        const ALPHABET: &[u8] = b"https://polymarket.com/event/sports/esports/market/es/ab-_%2e.:@?#\\ \t\n\0\xc3\xa9/";
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..20_000 {
            let len = (x % 64) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    ALPHABET[(x % ALPHABET.len() as u64) as usize]
                })
                .collect();
            let raw = String::from_utf8_lossy(&bytes);
            if let Ok(Link::Event(slug) | Link::Market(slug)) = parse_link(&raw) {
                assert!(valid_slug(&slug), "{raw:?}");
            }
        }
    }

    #[test]
    fn slug_and_locale_rules() {
        assert!(valid_slug("lol-blg-gen-2026-11-14"));
        assert!(valid_slug("a_b-C9"));
        for bad in ["", "a b", "a/b", "a.b", "a?b", "a#b", "a%20b", "é", "a\nb"] {
            assert!(!valid_slug(bad), "{bad:?}");
        }
        for ok in ["es", "zh", "pt-br", "EN"] {
            assert!(is_locale(ok), "{ok}");
        }
        for no in ["e", "esp", "pt_br", "p1", "event", "nba"] {
            assert!(!is_locale(no), "{no}");
        }
    }

    fn program(rate: &str, start: Option<&str>, end: Option<&str>) -> ClobReward {
        serde_json::from_value(serde_json::json!({
            "id": "1", "rewardsDailyRate": rate, "startDate": start, "endDate": end,
        }))
        .unwrap()
    }

    #[test]
    fn daily_pool_counts_only_programs_running_today() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
        let running = program("1905", Some("2026-11-13"), Some("2500-12-31"));
        let ended = program("700", Some("2026-10-01"), Some("2026-11-13"));
        let future = program("300", Some("2026-11-15"), None);
        let open = program("5", None, None);
        let zero = program("0", None, None);
        assert_eq!(daily_pool(&[running.clone(), ended, future, open, zero], today), dec!(1910));
        assert_eq!(daily_pool(&[], today), dec!(0));
        // The first and last day both count.
        let one_day = program("10", Some("2026-11-14"), Some("2026-11-14"));
        assert_eq!(daily_pool(&[one_day], today), dec!(10));
    }

    fn market(slug: &str, pool: Decimal, min: Option<Decimal>, spread: Option<Decimal>) -> MarketRef {
        MarketRef {
            condition_id: format!("0x{slug}"),
            question: slug.into(),
            group_item_title: None,
            market_slug: slug.into(),
            event_slug: "game".into(),
            image: None,
            outcomes: vec!["A".into(), "B".into()],
            token_ids: vec!["1".into(), "2".into()],
            outcome_prices: vec![dec!(0.5), dec!(0.5)],
            tick_size: dec!(0.01),
            rewards_min_size: min,
            rewards_max_spread: spread,
            reward_pool: pool,
            volume_24hr: None,
            liquidity: None,
            last_trade_price: None,
            one_day_price_change: None,
            end_date: None,
        }
    }

    #[test]
    fn has_rewards_needs_a_real_pool() {
        // Polymarket sets a min size and zone on most markets, pool or not.
        assert!(!market("x", dec!(0), Some(dec!(50)), Some(dec!(4.5))).has_rewards());
        assert!(market("x", dec!(1905), Some(dec!(500)), Some(dec!(2.5))).has_rewards());
        assert!(!market("x", dec!(1905), None, Some(dec!(2.5))).has_rewards());
        assert!(!market("x", dec!(1905), Some(dec!(500)), Some(dec!(0))).has_rewards());
    }

    #[test]
    fn picker_lists_rewarded_markets_first_with_the_main_market_on_top() {
        let r = |slug: &str, pool| market(slug, pool, Some(dec!(500)), Some(dec!(2.5)));
        let none = |slug: &str| market(slug, dec!(0), Some(dec!(50)), Some(dec!(4.5)));
        let mut refs = vec![
            none("game-baron"),
            r("game-kills", dec!(1143)),
            r("game-1", dec!(1905)),
            none("game-dragon"),
            r("game", dec!(1905)),
        ];
        sort_candidates(&mut refs, "game");
        let order: Vec<&str> = refs.iter().map(|m| m.market_slug.as_str()).collect();
        assert_eq!(order, ["game", "game-1", "game-kills", "game-baron", "game-dragon"]);
    }
}
