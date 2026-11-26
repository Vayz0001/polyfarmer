//! Who holds a market and who is trading it, from Polymarket's public Data API
//! (v2): the top holders of each outcome (`/holders`) and the latest trades
//! above a dollar size (`/trades`). Read-only and unauthenticated, shown on the
//! market page; nothing here feeds the trading engine.
//!
//! Everything from the API is untrusted: bodies are size-capped, only the
//! fields below are kept, display names are cleaned and shortened (the page
//! escapes them too), and a wallet is only linked when it is a well-formed
//! address.

use std::sync::LazyLock;
use std::time::Duration;

use eyre::{eyre, Result};
use rust_decimal::{Decimal, RoundingStrategy};
use rust_decimal_macros::dec;
use serde::de::DeserializeOwned;
use serde::Deserialize;

const DATA_API: &str = "https://data-api.polymarket.com/v2";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// A page of 10–20 rows is a few KB; anything far bigger is not what we asked for.
const MAX_BODY_BYTES: usize = 1 << 20;
/// Longest display name we show, in characters.
const NAME_MAX_CHARS: usize = 24;

/// Holders shown per outcome.
pub const HOLDERS_PER_OUTCOME: u32 = 10;
/// Trades shown per filter.
pub const TRADES_SHOWN: u32 = 10;
/// The trade-size filters offered on the market page, in dollars.
pub const TRADE_FILTERS: [u32; 4] = [100, 1_000, 10_000, 100_000];
pub const DEFAULT_TRADE_FILTER: u32 = 100;

/// Holds the build error text instead of panicking; each request reports it.
static HTTP: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // Stay on Polymarket's API host.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
});

/// One holder of one outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct Holder {
    /// Lower-case `0x…` address, or empty when the API sent something else.
    pub wallet: String,
    pub name: String,
    pub shares: Decimal,
}

/// The top holders of one outcome (`outcome_index` 0 or 1).
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeHolders {
    pub outcome_index: usize,
    pub holders: Vec<Holder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

/// One trade (taker side), newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub wallet: String,
    pub name: String,
    pub side: Side,
    pub outcome_index: usize,
    pub shares: Decimal,
    /// Price per share in dollars (0–1).
    pub price: Decimal,
    /// Unix seconds.
    pub timestamp: i64,
}

impl Trade {
    /// Dollars that changed hands (shares × price).
    pub fn usd(&self) -> Decimal {
        self.shares * self.price
    }
}

// ── Wire format (only the fields we use) ─────────────────────────────────────

#[derive(Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

#[derive(Deserialize)]
struct RawTokenHolders {
    #[serde(default = "Vec::new")]
    holders: Vec<RawHolder>,
}

#[derive(Deserialize)]
struct RawHolder {
    #[serde(default)]
    proxy_wallet: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    pseudonym: String,
    #[serde(default)]
    display_username_public: bool,
    #[serde(default)]
    amount: Decimal,
    #[serde(default)]
    outcome_index: u32,
}

#[derive(Deserialize)]
struct RawTrade {
    #[serde(default)]
    proxy_wallet: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    pseudonym: String,
    #[serde(default)]
    side: String,
    #[serde(default)]
    outcome_index: u32,
    #[serde(default)]
    size: Decimal,
    #[serde(default)]
    price: Decimal,
    #[serde(default)]
    timestamp: i64,
}

// ── Requests ─────────────────────────────────────────────────────────────────

/// `0x` + 64 hex digits.
pub fn valid_condition_id(c: &str) -> bool {
    c.len() == 66 && c.starts_with("0x") && c[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// The closest offered filter at or below `usd` (unknown values fall back to
/// the default), so the query string can't ask the API for anything else.
pub fn trade_filter(usd: Option<u32>) -> u32 {
    match usd {
        Some(v) if TRADE_FILTERS.contains(&v) => v,
        _ => DEFAULT_TRADE_FILTER,
    }
}

async fn get_json<T: DeserializeOwned>(path: &str, query: &[(&str, String)]) -> Result<T> {
    let http = HTTP.as_ref().map_err(|e| eyre!("HTTP client unavailable: {e}"))?;
    let mut resp = http.get(format!("{DATA_API}/{path}")).query(query).send().await?;
    if !resp.status().is_success() {
        return Err(eyre!("Polymarket answered HTTP {}", resp.status().as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(eyre!("Polymarket sent an unexpectedly large reply"));
        }
        body.extend_from_slice(&chunk);
    }
    // serde's message would quote the offending value; keep only where it was.
    serde_json::from_slice(&body)
        .map_err(|e| eyre!("unexpected reply from Polymarket (line {}, column {})", e.line(), e.column()))
}

/// The top holders of both outcomes of `condition_id`, in one request.
pub async fn top_holders(condition_id: &str) -> Result<Vec<OutcomeHolders>> {
    if !valid_condition_id(condition_id) {
        return Err(eyre!("not a market id"));
    }
    let page: Page<RawTokenHolders> =
        get_json("holders", &[("condition", condition_id.to_string()), ("limit", HOLDERS_PER_OUTCOME.to_string())])
            .await?;
    Ok(holders_from(page))
}

fn holders_from(page: Page<RawTokenHolders>) -> Vec<OutcomeHolders> {
    let mut out: Vec<OutcomeHolders> = Vec::new();
    for token in page.data {
        let Some(index) = token.holders.first().map(|h| h.outcome_index as usize) else { continue };
        if index > 1 || out.iter().any(|o| o.outcome_index == index) {
            continue;
        }
        let mut holders: Vec<Holder> = token
            .holders
            .into_iter()
            .filter(|h| h.outcome_index as usize == index && h.amount > Decimal::ZERO)
            .map(|h| Holder {
                name: display_name(&h.name, &h.pseudonym, h.display_username_public, &h.proxy_wallet),
                wallet: clean_wallet(&h.proxy_wallet),
                shares: h.amount,
            })
            .collect();
        holders.sort_by_key(|h| std::cmp::Reverse(h.shares));
        holders.truncate(HOLDERS_PER_OUTCOME as usize);
        out.push(OutcomeHolders { outcome_index: index, holders });
    }
    out.sort_by_key(|o| o.outcome_index);
    out
}

/// The latest trades in `condition_id` worth at least `min_usd`, newest first.
pub async fn recent_trades(condition_id: &str, min_usd: u32) -> Result<Vec<Trade>> {
    if !valid_condition_id(condition_id) {
        return Err(eyre!("not a market id"));
    }
    let page: Page<RawTrade> = get_json(
        "trades",
        &[
            ("condition", condition_id.to_string()),
            ("filter_type", "CASH".to_string()),
            ("filter_amount", trade_filter(Some(min_usd)).to_string()),
            ("limit", TRADES_SHOWN.to_string()),
        ],
    )
    .await?;
    Ok(trades_from(page, trade_filter(Some(min_usd))))
}

fn trades_from(page: Page<RawTrade>, min_usd: u32) -> Vec<Trade> {
    let mut trades: Vec<Trade> = page
        .data
        .into_iter()
        .filter_map(|t| {
            let side = match t.side.to_ascii_uppercase().as_str() {
                "BUY" => Side::Buy,
                "SELL" => Side::Sell,
                _ => return None,
            };
            if t.outcome_index > 1 || t.size <= Decimal::ZERO || t.price <= Decimal::ZERO || t.price > Decimal::ONE {
                return None;
            }
            Some(Trade {
                name: display_name(&t.name, &t.pseudonym, true, &t.proxy_wallet),
                wallet: clean_wallet(&t.proxy_wallet),
                side,
                outcome_index: t.outcome_index as usize,
                shares: t.size,
                price: t.price,
                timestamp: t.timestamp,
            })
        })
        // The API filters by size already; re-check so a misbehaving reply
        // can't put small trades under a "$10k+" heading.
        .filter(|t| t.usd() >= Decimal::from(min_usd))
        .collect();
    trades.sort_by_key(|t| std::cmp::Reverse(t.timestamp));
    trades.truncate(TRADES_SHOWN as usize);
    trades
}

// ── Display helpers ──────────────────────────────────────────────────────────

/// Lower-case address when it is `0x` + 40 hex digits, else empty.
fn clean_wallet(w: &str) -> String {
    let ok = w.len() == 42 && w.starts_with("0x") && w[2..].bytes().all(|b| b.is_ascii_hexdigit());
    if ok {
        w.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// What Polymarket shows for a trader: their username when public, else the
/// generated pseudonym, else a shortened address. Accounts without a username
/// get an auto name like `0xAbC…-1768216498538`, which is skipped too.
fn display_name(name: &str, pseudonym: &str, public: bool, wallet: &str) -> String {
    let name = clean_text(name);
    let auto = name.to_ascii_lowercase().starts_with("0x");
    if public && !name.is_empty() && !auto {
        return name;
    }
    let pseudonym = clean_text(pseudonym);
    if !pseudonym.is_empty() {
        return pseudonym;
    }
    let w = clean_wallet(wallet);
    if w.is_empty() {
        "anonymous".to_string()
    } else {
        format!("{}…{}", &w[..6], &w[38..])
    }
}

/// Printable characters only, trimmed, at most `NAME_MAX_CHARS` (with "…").
fn clean_text(s: &str) -> String {
    let s: String = s.chars().filter(|c| !c.is_control() && !is_invisible(*c)).collect();
    let s = s.trim();
    if s.chars().count() > NAME_MAX_CHARS {
        let cut: String = s.chars().take(NAME_MAX_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        s.to_string()
    }
}

/// Zero-width and direction-override characters, which can make a name
/// render as something else.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// Share counts: "940", "12.3k", "3.11M" (rounded, not truncated).
pub fn fmt_shares(s: Decimal) -> String {
    let r = |x: Decimal, dp: u32| x.round_dp_with_strategy(dp, RoundingStrategy::MidpointAwayFromZero);
    if s >= dec!(999_995) {
        format!("{}M", r(s / dec!(1_000_000), 2).normalize())
    } else if s >= dec!(999.95) {
        format!("{}k", r(s / dec!(1_000), 1).normalize())
    } else {
        r(s, 0).normalize().to_string()
    }
}

/// Filter label: "$100+", "$1k+", "$10k+", "$100k+".
pub fn filter_label(usd: u32) -> String {
    if usd >= 1_000 {
        format!("${}k+", usd / 1_000)
    } else {
        format!("${usd}+")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "0x25ff40795e25bb5f2a8dcc26f134ea81e224e0cf";

    #[test]
    fn holders_parse_both_outcomes_from_one_reply() {
        // Shape as returned by GET /v2/holders (trimmed), Nov 2026.
        let body = format!(
            r#"{{"data":[
              {{"token":"1","holders":[
                {{"proxy_wallet":"{ME}","pseudonym":"Hot-Candle","amount":852432.394775,"display_username_public":true,"outcome_index":0,"name":"0x25fF40795E25bB5F2a8DCC26F134EA81E224e0cf-1768216498538"}},
                {{"proxy_wallet":"0x1111111111111111111111111111111111111111","pseudonym":"Similar-Format","amount":454149.1,"display_username_public":true,"outcome_index":0,"name":"truthteller"}}
              ]}},
              {{"token":"2","holders":[
                {{"proxy_wallet":"0x2222222222222222222222222222222222222222","pseudonym":"Rapid","amount":3108608,"display_username_public":false,"outcome_index":1,"name":"hidden-name"}}
              ]}}
            ],"pagination":{{"has_more":true}}}}"#
        );
        let out = holders_from(serde_json::from_str(&body).unwrap());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].outcome_index, 0);
        // Auto-generated "0x…-timestamp" name → pseudonym; a real name is kept.
        assert_eq!(out[0].holders[0].name, "Hot-Candle");
        assert_eq!(out[0].holders[0].wallet, ME);
        assert_eq!(out[0].holders[1].name, "truthteller");
        // A name the holder keeps private is not shown.
        assert_eq!(out[1].holders[0].name, "Rapid");
        assert_eq!(out[1].holders[0].shares, dec!(3108608));
    }

    #[test]
    fn holders_ignore_junk_rows() {
        let body = r#"{"data":[
            {"holders":[]},
            {"holders":[{"proxy_wallet":"not-an-address","pseudonym":"","name":"","amount":5,"outcome_index":0},
                        {"proxy_wallet":"0x1","amount":0,"outcome_index":0},
                        {"proxy_wallet":"0x1","amount":7,"outcome_index":1}]},
            {"holders":[{"amount":9,"outcome_index":7}]}
        ]}"#;
        let out = holders_from(serde_json::from_str(body).unwrap());
        assert_eq!(out.len(), 1, "empty token and out-of-range outcome dropped");
        assert_eq!(out[0].holders.len(), 1, "zero balance and the other outcome's row dropped");
        assert_eq!(out[0].holders[0].wallet, "", "bad address is never linked");
        assert_eq!(out[0].holders[0].name, "anonymous");
    }

    #[test]
    fn trades_parse_filter_and_sort() {
        let body = format!(
            r#"{{"data":[
              {{"proxy_wallet":"{ME}","side":"SELL","outcome_index":1,"size":7197.01,"price":0.9637,"timestamp":100,"name":"prophetless","pseudonym":"Shimmering"}},
              {{"proxy_wallet":"{ME}","side":"BUY","outcome_index":0,"size":2000,"price":0.6,"timestamp":200,"name":"","pseudonym":"Quiet-Fox"}},
              {{"proxy_wallet":"{ME}","side":"BUY","outcome_index":0,"size":10,"price":0.5,"timestamp":300}},
              {{"proxy_wallet":"{ME}","side":"MERGE","outcome_index":0,"size":9000,"price":0.5,"timestamp":400}},
              {{"proxy_wallet":"{ME}","side":"BUY","outcome_index":0,"size":9000,"price":1.5,"timestamp":500}}
            ]}}"#
        );
        let out = trades_from(serde_json::from_str(&body).unwrap(), 1_000);
        assert_eq!(out.len(), 2, "under $1k, unknown side and impossible price dropped");
        assert_eq!(out[0].timestamp, 200, "newest first");
        assert_eq!(out[0].side, Side::Buy);
        assert_eq!(out[0].name, "Quiet-Fox");
        assert_eq!(out[1].side, Side::Sell);
        assert_eq!(out[1].usd().round_dp(2), dec!(6935.76));
    }

    #[test]
    fn names_are_cleaned_and_shortened() {
        assert_eq!(display_name("  bob\u{202E}\u{200B}\n ", "", true, ME), "bob");
        assert_eq!(display_name("", "", true, ME), "0x25ff…e0cf");
        assert_eq!(display_name("", "", true, "javascript:alert(1)"), "anonymous");
        let long = display_name("a-very-long-trader-name-that-goes-on", "", true, ME);
        assert_eq!(long.chars().count(), NAME_MAX_CHARS);
        assert!(long.ends_with('…'));
        // Hostile text survives only as text (the template escapes it).
        assert_eq!(display_name("<script>x</script>", "", true, ME), "<script>x</script>");
    }

    #[test]
    fn ids_and_filters_are_validated() {
        assert!(valid_condition_id(&format!("0x{}", "ab".repeat(32))));
        assert!(!valid_condition_id("0x12"));
        assert!(!valid_condition_id(&format!("0x{}&limit=999", "ab".repeat(28))));
        assert_eq!(trade_filter(Some(10_000)), 10_000);
        assert_eq!(trade_filter(Some(5)), DEFAULT_TRADE_FILTER);
        assert_eq!(trade_filter(None), DEFAULT_TRADE_FILTER);
        assert_eq!(TRADE_FILTERS.map(filter_label), ["$100+", "$1k+", "$10k+", "$100k+"]);
    }

    #[test]
    fn share_counts_round() {
        assert_eq!(fmt_shares(dec!(940.4)), "940");
        assert_eq!(fmt_shares(dec!(999.96)), "1k");
        assert_eq!(fmt_shares(dec!(12_349.9)), "12.3k");
        assert_eq!(fmt_shares(dec!(852_432.39)), "852.4k");
        assert_eq!(fmt_shares(dec!(3_108_608)), "3.11M");
    }
}

/// Against the live API (`cargo test -- --ignored`): a busy market has holders
/// on both sides and trades over $100.
#[cfg(test)]
mod live {
    #[tokio::test]
    #[ignore = "network"]
    async fn a_busy_market_has_holders_and_trades() {
        #[derive(serde::Deserialize)]
        struct M {
            #[serde(rename = "conditionId")]
            condition_id: String,
        }
        let url = "https://gamma-api.polymarket.com/markets?active=true&closed=false&order=volume24hr&ascending=false&limit=1";
        let m: Vec<M> = reqwest::get(url).await.unwrap().json().await.unwrap();
        let cid = &m[0].condition_id;
        let holders = super::top_holders(cid).await.unwrap();
        assert_eq!(holders.len(), 2, "{holders:?}");
        assert!(holders.iter().all(|o| !o.holders.is_empty() && o.holders.len() <= 10));
        let trades = super::recent_trades(cid, 100).await.unwrap();
        assert!(!trades.is_empty() && trades.len() <= 10);
        assert!(trades.iter().all(|t| t.usd() >= rust_decimal::Decimal::from(100)));
    }
}
