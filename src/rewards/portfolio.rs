//! Read-only portfolio data from Polymarket's public Data API (v2): open
//! positions, marked portfolio value, and wallet activity (fills, rewards…).
//!
//! Public + unauthenticated, keyed by the Polymarket (proxy/deposit) wallet
//! address — so it needs nothing from the trading engine and changes nothing
//! about how it trades. A plain `reqwest` client, like `markets_browse`.

use std::sync::LazyLock;
use std::time::Duration;

use eyre::Result;
use rust_decimal::Decimal;
use serde::Deserialize;

const DATA_API: &str = "https://data-api.polymarket.com/v2";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

static HTTP: LazyLock<reqwest::Client> =
    LazyLock::new(|| reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build().expect("static reqwest config"));

#[derive(Debug, Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

/// One open position (`GET /v2/positions?user=`).
#[derive(Debug, Clone, Deserialize)]
pub struct Position {
    #[serde(default)]
    pub condition_id: String,
    #[serde(default)]
    pub token_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub event_slug: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub current_size: Decimal,
    #[serde(default)]
    pub avg_price: Decimal,
    #[serde(default)]
    pub current_price: Decimal,
    #[serde(default)]
    pub current_value: Decimal,
    #[serde(default)]
    pub unrealized_pnl: Decimal,
    #[serde(default)]
    pub total_pnl: Decimal,
    #[serde(default)]
    pub percent_pnl: Decimal,
    #[serde(default)]
    pub redeemable: bool,
    #[serde(default)]
    pub end_date: Option<String>,
}

/// One wallet activity entry (`GET /v2/activity?user=`): TRADE, REWARD,
/// REDEEM, MERGE, SPLIT, DEPOSIT…
#[derive(Debug, Clone, Deserialize)]
pub struct Activity {
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default)]
    pub condition_id: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub size: Decimal,
    #[serde(default)]
    pub usdc_size: Decimal,
    #[serde(default)]
    pub price: Decimal,
    #[serde(default)]
    pub side: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub event_slug: String,
    #[serde(default)]
    pub transaction_hash: String,
}

/// A page with its pagination block (activity is cursor-paged).
#[derive(Debug, Deserialize)]
struct CursorPage<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
    #[serde(default)]
    pagination: Pagination,
}

#[derive(Debug, Default, Deserialize)]
struct Pagination {
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// Rewards are one payout per day, so even years of history is a few hundred
/// rows — but never loop unboundedly on an upstream that misbehaves.
const REWARD_PAGE_LIMIT: u32 = 500;
const REWARD_MAX_PAGES: usize = 20;

/// Every liquidity-reward payout ever paid to `user` (newest first), from the
/// activity feed's `REWARD` entries. NB: the cursor query parameter is
/// `cursor` — the response field is `next_cursor`, but sending that name back
/// is silently ignored and returns page 1 again.
pub async fn reward_payouts(user: &str) -> Result<Vec<Activity>> {
    let mut all: Vec<Activity> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..REWARD_MAX_PAGES {
        let mut req = HTTP.get(format!("{DATA_API}/activity")).query(&[
            ("user", user),
            ("type", "REWARD"),
            ("limit", &REWARD_PAGE_LIMIT.to_string()),
        ]);
        if let Some(c) = &cursor {
            req = req.query(&[("cursor", c)]);
        }
        let page = req.send().await?.error_for_status()?.json::<CursorPage<Activity>>().await?;
        // Stop if a page adds nothing new (cursor ignored / upstream looping).
        let mut fresh = 0;
        for a in page.data {
            if a.kind == "REWARD" && seen.insert(format!("{}:{}", a.transaction_hash, a.timestamp)) {
                all.push(a);
                fresh += 1;
            }
        }
        match page.pagination.next_cursor {
            Some(c) if page.pagination.has_more && fresh > 0 => cursor = Some(c),
            _ => break,
        }
    }
    Ok(all)
}

/// All-time reward summary over a set of payouts.
pub struct RewardTotals {
    pub total: Decimal,
    pub payouts: usize,
    /// Unix seconds of the earliest payout.
    pub first_ts: Option<i64>,
}

pub fn summarize_rewards(payouts: &[Activity]) -> RewardTotals {
    RewardTotals {
        total: payouts.iter().map(|a| a.usdc_size).sum(),
        payouts: payouts.len(),
        first_ts: payouts.iter().map(|a| a.timestamp).min(),
    }
}

#[derive(Debug, Deserialize)]
struct ValueResp {
    data: ValueData,
}
#[derive(Debug, Deserialize)]
struct ValueData {
    #[serde(default)]
    value: Decimal,
}

/// Open positions for `user`, largest current value first (up to `limit`).
pub async fn positions(user: &str, limit: u32) -> Result<Vec<Position>> {
    let resp = HTTP
        .get(format!("{DATA_API}/positions"))
        .query(&[("user", user), ("limit", &limit.to_string())])
        .send()
        .await?
        .error_for_status()?;
    let mut rows = resp.json::<Page<Position>>().await?.data;
    rows.retain(|p| p.current_size > Decimal::ZERO);
    rows.sort_by_key(|p| std::cmp::Reverse(p.current_value));
    Ok(rows)
}

/// Marked value of all open positions, in USDC.
pub async fn value(user: &str) -> Result<Decimal> {
    let resp = HTTP.get(format!("{DATA_API}/value")).query(&[("user", user)]).send().await?.error_for_status()?;
    Ok(resp.json::<ValueResp>().await?.data.value)
}

/// Most recent wallet activity (newest first), up to `limit` entries.
pub async fn activity(user: &str, limit: u32) -> Result<Vec<Activity>> {
    let resp = HTTP
        .get(format!("{DATA_API}/activity"))
        .query(&[("user", user), ("limit", &limit.to_string())])
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json::<Page<Activity>>().await?.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_v2_shapes() {
        // Trimmed from real responses (Oct 2026).
        let pos = r#"{"data":[{"proxy_wallet":"0x1","token_id":"333","condition_id":"0xb1",
            "current_size":2808.09,"avg_price":0.43,"current_price":0.435,"current_value":1221.5191,
            "unrealized_pnl":14.0404,"total_pnl":14.0404,"percent_pnl":1.1627,"status":"OPEN",
            "redeemable":false,"title":"Penn State vs. Northwestern","slug":"cfb","outcome":"Northwestern",
            "end_date":"2026-10-03"}],"pagination":{"limit":1,"has_more":false,"next_cursor":null}}"#;
        let p: Page<Position> = serde_json::from_str(pos).unwrap();
        assert_eq!(p.data[0].current_value.to_string(), "1221.5191");
        assert_eq!(p.data[0].outcome, "Northwestern");

        let act = r#"{"data":[{"timestamp":1790919881,"condition_id":"0x45","type":"TRADE",
            "size":10.5,"usdc_size":4.2,"price":0.4,"side":"BUY","outcome":"Yes","title":"T",
            "slug":"s","transaction_hash":"0xab"}],"pagination":{}}"#;
        let a: Page<Activity> = serde_json::from_str(act).unwrap();
        assert_eq!(a.data[0].kind, "TRADE");
        assert_eq!(a.data[0].side, "BUY");

        let page: CursorPage<Activity> = serde_json::from_str(
            r#"{"data":[{"timestamp":10,"type":"REWARD","usdc_size":1.5,"transaction_hash":"0xa"},
                        {"timestamp":20,"type":"REWARD","usdc_size":2.25,"transaction_hash":"0xb"}],
                "pagination":{"has_more":true,"next_cursor":"abc"}}"#,
        )
        .unwrap();
        assert!(page.pagination.has_more);
        assert_eq!(page.pagination.next_cursor.as_deref(), Some("abc"));
        let t = summarize_rewards(&page.data);
        assert_eq!(t.total.to_string(), "3.75");
        assert_eq!((t.payouts, t.first_ts), (2, Some(10)));

        let v: ValueResp = serde_json::from_str(r#"{"data":{"proxy_wallet":"0x1","value":2481.2971}}"#).unwrap();
        assert_eq!(v.data.value.to_string(), "2481.2971");
    }
}
