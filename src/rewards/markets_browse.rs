//! Browse reward-eligible markets via `GET /rewards/markets/multi` — public,
//! unauthenticated, purpose-built search/sort/filter/pagination. Not wrapped
//! by the SDK (which only wraps `/rewards/markets/current`), so this is a
//! plain `reqwest` call rather than a gap to work around.

use eyre::Result;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::time::Duration;

const REWARDS_MULTI_URL: &str = "https://clob.polymarket.com/rewards/markets/multi";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
pub struct RewardsMultiResponse {
    pub count: u32,
    pub next_cursor: Option<String>,
    pub data: Vec<RewardsMultiMarket>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RewardsMultiMarket {
    pub condition_id: String,
    pub event_slug: String,
    pub market_slug: String,
    pub image: Option<String>,
    pub market_competitiveness: Option<Decimal>,
    pub one_day_price_change: Option<Decimal>,
    pub question: String,
    pub rewards_max_spread: Decimal,
    pub rewards_min_size: Decimal,
    pub spread: Decimal,
    pub end_date: Option<String>,
    pub volume_24hr: Option<Decimal>,
    #[serde(default)]
    pub tokens: Vec<RewardsMultiToken>,
    #[serde(default)]
    pub rewards_config: Vec<RewardsMultiConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RewardsMultiToken {
    pub token_id: String,
    pub outcome: String,
    pub price: Decimal,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RewardsMultiConfig {
    pub rate_per_day: Decimal,
}

/// `order_by`: one of "rate_per_day" | "competitiveness" | "spread" |
/// "volume_24hr" | "one_day_price_change" | "end_date" | "question" | ... per
/// the official API reference. `position`: "ASC" | "DESC".
#[derive(Debug, Default)]
pub struct BrowseQuery {
    pub q: Option<String>,
    pub order_by: Option<String>,
    pub position: Option<String>,
    pub page_size: Option<u32>,
    pub next_cursor: Option<String>,
}

pub async fn browse(query: &BrowseQuery) -> Result<RewardsMultiResponse> {
    let client = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build()?;
    let mut req = client.get(REWARDS_MULTI_URL);
    if let Some(q) = &query.q {
        req = req.query(&[("q", q)]);
    }
    if let Some(v) = &query.order_by {
        req = req.query(&[("order_by", v)]);
    }
    if let Some(v) = &query.position {
        req = req.query(&[("position", v)]);
    }
    if let Some(v) = query.page_size {
        req = req.query(&[("page_size", v)]);
    }
    if let Some(v) = &query.next_cursor {
        req = req.query(&[("next_cursor", v)]);
    }
    let resp = req.send().await?.error_for_status()?;
    Ok(resp.json().await?)
}
