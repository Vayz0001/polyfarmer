use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as _;
use chrono::NaiveDate;
use eyre::Result;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::clob::types::request::UserRewardsEarningRequest;
use polymarket_client_sdk_v2::clob::types::response::UserRewardsEarningResponse;
use polymarket_client_sdk_v2::clob::types::{OrderStatusType, SignatureType};
use polymarket_client_sdk_v2::clob::{Client, Config as ClobConfig};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use tracing::{error, info, warn};

const CLOB_URL: &str = "https://clob.polymarket.com";
const POLYGON_CHAIN_ID: u64 = 137;
const TERMINAL_CURSOR: &str = "LTE="; // base64("-1") — signals end of pagination
const RETRY_ATTEMPTS: u32 = 3;
// The retry loops below make at least one attempt, so `last_err` is always set when they give up.
const _: () = assert!(RETRY_ATTEMPTS >= 1);

// Time limits on calls to Polymarket. The SDK's HTTP client has none, so a
// stalled call would otherwise wedge the heartbeat, a placement or a cancel.
// Each retry attempt gets its own limit. A timeout's message says "timed out",
// which the placer treats as an outage (placer::is_transient), not a rejection.
const HEARTBEAT_LIMIT: Duration = Duration::from_secs(5);
const CANCEL_LIMIT: Duration = Duration::from_secs(10);
const LIST_LIMIT: Duration = Duration::from_secs(10);
const PLACE_LIMIT: Duration = Duration::from_secs(15);
const READ_LIMIT: Duration = Duration::from_secs(10);
const REWARDS_LIMIT: Duration = Duration::from_secs(20);
const SIGN_IN_LIMIT: Duration = Duration::from_secs(30);

/// Run one call to Polymarket with a time limit.
async fn bounded<T, E, F>(what: &str, limit: Duration, call: F) -> Result<T>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
    E: Into<eyre::Report>,
{
    match tokio::time::timeout(limit, call).await {
        Ok(r) => r.map_err(Into::into),
        Err(_) => Err(eyre::eyre!("{what} timed out after {}s", limit.as_secs())),
    }
}
const RETRY_BASE_MS: u64 = 200;

/// Our token ids are decimal strings; the SDK takes a typed U256.
fn parse_token(token_id: &str) -> Result<U256> {
    U256::from_str(token_id).map_err(|e| eyre::eyre!("invalid token_id {token_id}: {e}"))
}

type AuthClient = Client<Authenticated<Normal>>;

pub struct Executor {
    pub client: AuthClient,
    pub signer: PrivateKeySigner,
}

impl Executor {
    /// Authenticate with the Polymarket V2 CLOB using decrypted wallet creds.
    ///
    /// The order's `signatureType` must match how the funder wallet was created,
    /// or the CLOB rejects it. We pick it from the funder address:
    ///   * `funder == EOA`            → `Eoa` (trade directly, no funder)
    ///   * on-chain Deposit Wallet    → `Poly1271` (EIP-1271 / ERC-7739; the
    ///     current Polymarket default, handled natively by SDK >= 0.7)
    ///   * derived Magic/proxy wallet → `Proxy`
    ///   * otherwise                  → `GnosisSafe` (browser-wallet Safe)
    ///
    /// `rpc_url` is the Polygon RPC used only to classify the funder (one
    /// `eth_call`); `None` uses the public fallback list.
    pub async fn new(private_key: &str, proxy_wallet: Address, rpc_url: Option<&str>) -> Result<Self> {
        use polymarket_client_sdk_v2::{derive_proxy_wallet, POLYGON};

        let signer: PrivateKeySigner = private_key.parse()?;
        let signer = signer.with_chain_id(Some(POLYGON_CHAIN_ID));
        let eoa = signer.address();
        info!("Signer address: {eoa}");

        let sig_type = if proxy_wallet == eoa {
            SignatureType::Eoa
        } else {
            // Never guess: the wrong signature type gets every order rejected
            // for the whole session. If Polygon can't be asked, fail this start;
            // the engine retries it shortly.
            let deposit = crate::wallet_detect::deposit_wallet_check(proxy_wallet, rpc_url).await.map_err(|e| {
                eyre::eyre!(
                    "couldn't check your Polymarket wallet's type on Polygon ({e}). Orders signed with the wrong \
                     type would all be rejected, so the engine retries shortly. A reliable POLYGON_RPC_URL avoids this."
                )
            })?;
            if deposit {
                SignatureType::Poly1271
            } else if Some(proxy_wallet) == derive_proxy_wallet(eoa, POLYGON) {
                SignatureType::Proxy
            } else {
                SignatureType::GnosisSafe
            }
        };
        info!("Order maker {proxy_wallet} → signatureType {sig_type:?}");

        // A funder with an EOA signature type is rejected by the SDK, so an EOA
        // wallet trades directly (no funder); every managed wallet funds via the
        // proxy / safe / deposit-wallet flow.
        let auth = Client::new(CLOB_URL, ClobConfig::default())?.authentication_builder(&signer);
        let auth = match sig_type {
            SignatureType::Eoa => auth.signature_type(SignatureType::Eoa),
            other => auth.funder(proxy_wallet).signature_type(other),
        };
        let client = bounded("signing in to Polymarket", SIGN_IN_LIMIT, auth.authenticate()).await?;

        info!("Authenticated with Polymarket CLOB ({sig_type:?})");
        Ok(Self { client, signer })
    }

    /// Compute share quantity from a USD order size and a price.
    /// Returns an error if price is zero or negative to prevent division by zero / nonsensical orders.
    pub fn shares_from_usd(order_size_usd: Decimal, price: Decimal) -> Result<Decimal> {
        if price <= Decimal::ZERO {
            eyre::bail!("Cannot compute shares: price is {} (must be > 0)", price);
        }
        Ok((order_size_usd / price).round_dp(2))
    }

    /// Tick size the order library validates prices against. The SDK caches it
    /// per token for the life of the process, so it can be stale.
    pub async fn sdk_tick(&self, token_id: &str) -> Result<Decimal> {
        let token = parse_token(token_id)?;
        Ok(bounded("reading the tick size", READ_LIMIT, self.client.tick_size(token))
            .await?
            .minimum_tick_size
            .as_decimal())
    }

    /// Read the current tick size from Polymarket, replacing the SDK's cached
    /// value. The SDK only offers clearing its caches together: tick sizes and
    /// neg-risk flags (and legacy fee rates) are refetched on each market's next
    /// order. Market fee info is kept.
    pub async fn refresh_tick(&self, token_id: &str) -> Result<Decimal> {
        let token = parse_token(token_id)?;
        self.client.invalidate_internal_caches();
        Ok(bounded("reading the tick size", READ_LIMIT, self.client.tick_size(token))
            .await?
            .minimum_tick_size
            .as_decimal())
    }

    /// Tell the order library about a tick size we learned elsewhere (the
    /// market WebSocket). Only Polymarket's known tick sizes are accepted.
    pub fn set_tick(&self, token_id: &str, tick: Decimal) -> Result<()> {
        use polymarket_client_sdk_v2::clob::types::TickSize;
        let token = parse_token(token_id)?;
        let tick = TickSize::try_from(tick).map_err(|e| eyre::eyre!("unknown tick size {tick}: {e}"))?;
        self.client.set_tick_size(token, tick);
        Ok(())
    }

    /// Place a resting GTC BUY order. Returns the order ID.
    /// Price must already be snapped to tick size before calling.
    pub async fn place_buy_order(&self, token_id: &str, price: Decimal, size: Decimal) -> Result<String> {
        use polymarket_client_sdk_v2::clob::types::Side;

        let token_id_u256 = parse_token(token_id)?;
        // The SDK compares decimal places, not value: 0.210 fails on a 0.01
        // tick although it is 0.21. Drop trailing zeros first.
        let price = price.normalize();

        // One limit over building (may read market info), signing and posting.
        let response = bounded("placing the order", PLACE_LIMIT, async {
            let order = self
                .client
                .limit_order()
                .token_id(token_id_u256)
                .price(price)
                .size(size)
                .side(Side::Buy)
                .build()
                .await?;
            let signed = self.client.sign(&self.signer, order).await?;
            Ok::<_, eyre::Report>(self.client.post_order(signed).await?)
        })
        .await?;
        let order_id = accepted_order_id(response.success, &response.order_id, response.error_msg.as_deref())?;

        info!("Order placed: {} @ {} size {}", order_id, price, size);
        Ok(order_id)
    }

    /// Cancel a single order by ID.
    /// Retries up to RETRY_ATTEMPTS times on transient errors (cancel is idempotent).
    /// Returns true if confirmed cancelled, false otherwise (logs reason if available).
    pub async fn cancel_order(&self, order_id: &str) -> Result<bool> {
        let mut last_err = None;
        for attempt in 1..=RETRY_ATTEMPTS {
            // Use the bulk endpoint — the singular DELETE /order endpoint returns
            // "can't be found" even for live orders (confirmed Polymarket bug).
            // The bulk endpoint (DELETE /orders) works correctly.
            match bounded("cancelling the order", CANCEL_LIMIT, self.client.cancel_orders(&[order_id])).await {
                Ok(response) => {
                    let id_lower = order_id.to_lowercase();
                    let cancelled = response.canceled.iter().any(|id| id.to_lowercase() == id_lower);
                    if cancelled {
                        return Ok(true);
                    }

                    // Look for the order ID as the key (normal Polymarket response).
                    let keyed_reason = response.not_canceled.get(order_id).or_else(|| {
                        response.not_canceled.iter().find(|(k, _)| k.to_lowercase() == id_lower).map(|(_, v)| v)
                    });

                    if let Some(reason) = keyed_reason {
                        // "already canceled or matched" means the order is gone — treat as success.
                        if reason.contains("already canceled") || reason.contains("already matched") {
                            info!("Order {} already gone ({})", order_id, reason);
                            return Ok(true);
                        }
                        warn!("Order {} NOT cancelled — reason: {}", order_id, reason);
                        return Ok(false);
                    }

                    // Ambiguous response — Polymarket sometimes returns not_canceled
                    // with an empty-string key ("can't be found") even for orders that are
                    // still active on the CLOB. Do NOT treat as confirmed cancel here.
                    // Callers that need certainty should use cancel_order_verified().
                    warn!(
                        "Order {} status unclear — canceled={:?} not_canceled={:?}",
                        order_id, response.canceled, response.not_canceled
                    );
                    return Ok(false);
                }
                Err(e) => {
                    warn!("cancel_order attempt {}/{}: {}", attempt, RETRY_ATTEMPTS, e);
                    last_err = Some(e);
                    if attempt < RETRY_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(RETRY_BASE_MS * attempt as u64)).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| eyre::eyre!("no attempt was made")))
    }

    /// Cancel an order and verify the result via GET /order when the cancel
    /// response is ambiguous (i.e. cancel_order returned Ok(false)).
    ///
    /// Returns:
    ///   Ok(true)  — order confirmed off the CLOB: cancelled, or GET /order says
    ///               it doesn't exist (404) or is MATCHED / CANCELED
    ///   Ok(false) — GET /order shows it still resting and a second cancel didn't
    ///               confirm either
    ///   Err(_)    — couldn't tell (network / API failure, including a failed
    ///               GET /order)
    ///
    /// Only Ok(true) frees a leg: anything else leaves it Cancelling and the
    /// engine's 30s timer retries, so an order that may still rest is never
    /// forgotten (which could leave two orders up, or one up on a paused leg).
    pub async fn cancel_order_verified(&self, order_id: &str) -> Result<bool> {
        if self.cancel_order(order_id).await? {
            return Ok(true);
        }
        info!("cancel_order ambiguous for {} — verifying via GET /order", order_id);
        let looked = match tokio::time::timeout(CANCEL_LIMIT, self.client.order(order_id)).await {
            Ok(r) => r,
            Err(_) => return Err(eyre::eyre!("checking order {order_id} timed out after {}s", CANCEL_LIMIT.as_secs())),
        };
        match looked {
            Err(e) if is_not_found(&e) => {
                info!("Order {} confirmed gone via GET /order", order_id);
                Ok(true)
            }
            Err(e) => Err(eyre::eyre!("could not check order {order_id} after an unclear cancel: {e}")),
            Ok(o) if is_off_book(&o.status) => {
                info!("Order {} already off the book ({:?})", order_id, o.status);
                Ok(true)
            }
            Ok(o) => {
                warn!("Order {} still {:?} per GET /order — retrying cancel", order_id, o.status);
                match self.cancel_order(order_id).await {
                    Ok(true) => Ok(true),
                    Ok(false) => {
                        // Possibly mid-match (Polymarket can answer "can't be found" while the
                        // record still says live). Not treated as gone: the timer retries, and
                        // a later GET shows it MATCHED or not found.
                        warn!("Order {} still resting after two cancel attempts — will retry", order_id);
                        Ok(false)
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Cancel a specific list of order IDs (bot-managed orders only).
    /// Uses the bulk SDK endpoint — one API call for all IDs. Retries on transient errors.
    ///
    /// Returns the IDs that are **still open** (see [`still_open`]): callers must
    /// only treat the others as gone, or the bot would place over orders that are
    /// still resting.
    pub async fn cancel_orders(&self, order_ids: &[String]) -> Result<Vec<String>> {
        if order_ids.is_empty() {
            info!("cancel_orders: nothing to cancel");
            return Ok(Vec::new());
        }
        info!("cancel_orders: cancelling {} orders", order_ids.len());
        let ids_ref: Vec<&str> = order_ids.iter().map(|s| s.as_str()).collect();
        let mut last_err = None;
        for attempt in 1..=RETRY_ATTEMPTS {
            match bounded("cancelling orders", CANCEL_LIMIT, self.client.cancel_orders(&ids_ref)).await {
                Ok(response) => {
                    for id in &response.canceled {
                        info!("Cancelled order {}", id);
                    }
                    for (id, reason) in &response.not_canceled {
                        if already_gone(reason) {
                            info!("Order {} already gone ({})", id, reason);
                        } else {
                            warn!("Order {} NOT cancelled — reason: {}", id, reason);
                        }
                    }
                    return Ok(still_open(order_ids, &response.canceled, &response.not_canceled));
                }
                Err(e) => {
                    warn!("cancel_orders attempt {}/{}: {}", attempt, RETRY_ATTEMPTS, e);
                    last_err = Some(e);
                    if attempt < RETRY_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(RETRY_BASE_MS * attempt as u64)).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| eyre::eyre!("no attempt was made")))
    }

    /// Fetch all open order IDs for a single token_id, handling pagination.
    /// Retries each page fetch on transient errors.
    pub async fn fetch_open_order_ids(&self, token_id: &str) -> Result<Vec<String>> {
        use polymarket_client_sdk_v2::clob::types::request::OrdersRequest;
        let asset_id = U256::from_str(token_id).map_err(|e| eyre::eyre!("invalid token_id {token_id}: {e}"))?;
        let req = OrdersRequest::builder().asset_id(asset_id).build();
        let mut ids = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        loop {
            let mut last_err = None;
            let page = 'retry: {
                for attempt in 1..=RETRY_ATTEMPTS {
                    match bounded("listing open orders", LIST_LIMIT, self.client.orders(&req, cursor.clone())).await {
                        Ok(p) => break 'retry p,
                        Err(e) => {
                            warn!("orders() attempt {}/{}: {}", attempt, RETRY_ATTEMPTS, e);
                            last_err = Some(e);
                            if attempt < RETRY_ATTEMPTS {
                                tokio::time::sleep(Duration::from_millis(RETRY_BASE_MS * attempt as u64)).await;
                            }
                        }
                    }
                }
                return Err(eyre::eyre!("{}", last_err.unwrap_or_else(|| eyre::eyre!("no attempt was made"))));
            };
            for order in &page.data {
                ids.push(order.id.clone());
            }
            pages += 1;
            match next_page(&page.next_cursor, pages) {
                Ok(Some(next)) => cursor = Some(next),
                Ok(None) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(ids)
    }

    /// On startup: fetch and cancel any open orders on the given token_ids.
    /// Only touches bot-tracked tokens — manual orders on other markets are untouched.
    pub async fn cancel_orders_for_tokens(&self, token_ids: &[String]) -> Result<()> {
        if token_ids.is_empty() {
            info!("cancel_orders_for_tokens: no tracked tokens, skipping");
            return Ok(());
        }
        info!("cancel_orders_for_tokens: checking {} tokens for open orders", token_ids.len());
        let mut listings = Vec::with_capacity(token_ids.len());
        for token_id in token_ids {
            let listing = self.fetch_open_order_ids(token_id).await;
            match &listing {
                Ok(ids) => info!("  token {}...: {} open orders", token_id.get(..8).unwrap_or(token_id), ids.len()),
                Err(e) => {
                    error!("Failed to fetch orders for token {}...: {}", token_id.get(..8).unwrap_or(token_id), e)
                }
            }
            listings.push(listing);
        }
        // Cancel what was found, then fail if any market couldn't be listed:
        // its old orders may still be resting, and trading beside them is what
        // this check exists to prevent. The caller retries the whole start.
        let (all_ids, unlisted) = gather_listings(listings);
        let open = self.cancel_orders(&all_ids).await?;
        if unlisted > 0 {
            return Err(eyre::eyre!("could not list the open orders of {unlisted} market(s)"));
        }
        if !open.is_empty() {
            return Err(eyre::eyre!("{} open order(s) could not be cancelled", open.len()));
        }
        Ok(())
    }

    /// Connectivity + auth health check.
    /// Calls GET /api-keys (L2-authenticated) every 5s.
    /// This verifies both network reachability and that our credentials are still valid.
    /// 3 consecutive failures trigger selective cancel of bot-managed orders as a safety net.
    /// Note: GTC orders do NOT expire without this call — it is purely a watchdog.
    pub async fn send_heartbeat(&self) -> Result<()> {
        bounded("the heartbeat", HEARTBEAT_LIMIT, self.client.api_keys()).await?;
        Ok(())
    }

    // ── Reward data (read-only, display) ────────────────────────────────────
    // No retry/backoff here, unlike the order-management methods above — these
    // back dashboard reads, not money-moving actions. A transient failure just
    // propagates to the caller (the web handler shows "try again").

    /// Batch scoring check — one call for the whole markets table instead of
    /// one per row.
    pub async fn are_orders_scoring(&self, order_ids: &[&str]) -> Result<HashMap<String, bool>> {
        bounded("checking scoring", READ_LIMIT, self.client.are_orders_scoring(order_ids)).await
    }

    /// Your reward-eligible markets with both the market's reward config and
    /// your own earning_percentage/earnings for the given date.
    ///
    /// Unfiltered, this endpoint returns *every* reward-eligible market on the
    /// platform (14,000+) with your (mostly-zero) numbers attached, and the
    /// SDK's typed request doesn't expose an only-my-markets filter. So it is
    /// walked twice, each sorted descending and stopped at the first market
    /// that doesn't matter: by `earnings` (markets that paid you today,
    /// including ones you have since paused or left) and by
    /// `earning_percentage` (markets you hold a share in right now, including
    /// ones that haven't paid yet). The two lists are merged. Sorting by share
    /// alone lost paused markets: their share drops to 0 and they sink among
    /// thousands of zero rows, past the page cap.
    pub async fn user_earnings_and_markets_config(&self, date: NaiveDate) -> Result<Vec<UserRewardsEarningResponse>> {
        let (paid, sharing) = tokio::try_join!(
            self.reward_markets_walk(date, "earnings", has_earnings),
            self.reward_markets_walk(date, "earning_percentage", has_share),
        )?;
        Ok(merge_reward_markets(paid, sharing))
    }

    /// One descending walk of `rewards/user/markets`, keeping the rows for
    /// which `relevant` holds and stopping at the first one that doesn't (the
    /// rest of a descending sort can't be relevant either), or at the page cap.
    async fn reward_markets_walk(
        &self,
        date: NaiveDate,
        order_by: &str,
        relevant: fn(&UserRewardsEarningResponse) -> bool,
    ) -> Result<Vec<UserRewardsEarningResponse>> {
        const MAX_PAGES: u32 = 5;
        let request = UserRewardsEarningRequest::builder().date(date).order_by(order_by).position("DESC").build();
        let mut kept = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let page = bounded(
                "reading rewards",
                REWARDS_LIMIT,
                self.client.user_earnings_and_markets_config(&request, cursor.clone()),
            )
            .await?;
            let reached_end = page.data.iter().any(|e| !relevant(e));
            kept.extend(page.data.into_iter().filter(relevant));
            if reached_end || page.next_cursor == TERMINAL_CURSOR {
                break;
            }
            cursor = Some(page.next_cursor);
        }
        Ok(kept)
    }

    /// Collateral (pUSD) balance available to the exchange, in USD. Read-only;
    /// the CLOB reports collateral in 6-decimal base units.
    pub async fn collateral_balance(&self) -> Result<Decimal> {
        use polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest;
        use polymarket_client_sdk_v2::clob::types::AssetType;
        let req = BalanceAllowanceRequest::builder().asset_type(AssetType::Collateral).build();
        let resp = bounded("reading the balance", READ_LIMIT, self.client.balance_allowance(req)).await?;
        Ok(resp.balance / Decimal::from(1_000_000u32))
    }
}

fn has_earnings(e: &UserRewardsEarningResponse) -> bool {
    e.earnings.iter().map(|a| a.earnings).sum::<Decimal>() > Decimal::ZERO
}

fn has_share(e: &UserRewardsEarningResponse) -> bool {
    e.earning_percentage > Decimal::ZERO
}

/// Union of the two reward-market walks, one row per market (`paid` first).
fn merge_reward_markets(
    paid: Vec<UserRewardsEarningResponse>,
    sharing: Vec<UserRewardsEarningResponse>,
) -> Vec<UserRewardsEarningResponse> {
    let mut merged = paid;
    for e in sharing {
        if !merged.iter().any(|m| m.condition_id == e.condition_id) {
            merged.push(e);
        }
    }
    merged
}

/// GET /order failed because the order doesn't exist: a 404, or the empty
/// reply the SDK also reports as a 404. A timeout or server error is not this.
fn is_not_found(e: &polymarket_client_sdk_v2::error::Error) -> bool {
    use polymarket_client_sdk_v2::error::{Kind, Status, StatusCode};
    e.kind() == Kind::Status && e.downcast_ref::<Status>().is_some_and(|s| s.status_code == StatusCode::NOT_FOUND)
}

/// Statuses that mean the order no longer rests on the book. Anything else
/// (LIVE, DELAYED, UNMATCHED, or a status we don't know) counts as resting.
fn is_off_book(status: &OrderStatusType) -> bool {
    matches!(status, OrderStatusType::Matched | OrderStatusType::Canceled)
}

/// The order id from a place reply, or an error with Polymarket's message.
/// The reply can arrive as HTTP 200 with `success: false` (and an empty id);
/// that is a rejection, not a placed order — treating it as placed left the
/// leg "live" on an order that doesn't exist, stuck cancelling it forever. A
/// refusal for funds keeps its text, so it reaches the "No balance" handling.
fn accepted_order_id(success: bool, order_id: &str, error_msg: Option<&str>) -> Result<String> {
    let id = order_id.trim();
    if success && !id.is_empty() {
        return Ok(id.to_string());
    }
    let why = error_msg.map(str::trim).filter(|m| !m.is_empty()).unwrap_or("no order id in Polymarket's reply");
    Err(eyre::eyre!("order rejected: {why}"))
}

/// Most pages of open orders read for one token before giving up (a cursor
/// that never ends would otherwise loop forever).
const MAX_ORDER_PAGES: usize = 50;

/// The next page to read after `cursor`, given `pages` read so far: `None` at
/// the end (Polymarket's `LTE=`, or an empty cursor), an error past
/// [`MAX_ORDER_PAGES`].
fn next_page(cursor: &str, pages: usize) -> Result<Option<String>> {
    if cursor.is_empty() || cursor == TERMINAL_CURSOR {
        return Ok(None);
    }
    if pages >= MAX_ORDER_PAGES {
        return Err(eyre::eyre!("open orders still not exhausted after {MAX_ORDER_PAGES} pages"));
    }
    Ok(Some(cursor.to_string()))
}

/// All order IDs found across the markets' listings, and how many markets
/// couldn't be listed.
fn gather_listings(listings: Vec<Result<Vec<String>>>) -> (Vec<String>, usize) {
    let mut ids = Vec::new();
    let mut failed = 0;
    for l in listings {
        match l {
            Ok(found) => ids.extend(found),
            Err(_) => failed += 1,
        }
    }
    (ids, failed)
}

/// A cancel refusal that means the order is off the book anyway.
fn already_gone(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.contains("already canceled") || r.contains("already cancelled") || r.contains("already matched")
}

/// Which of `requested` are still open after a bulk cancel answered with
/// `canceled` and `not_canceled` (id → reason). Gone: cancelled now, or already
/// cancelled or matched. Anything else — refused for another reason, or missing
/// from the answer — counts as still open (the cautious reading).
pub fn still_open(
    requested: &[String],
    canceled: &[String],
    not_canceled: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    requested
        .iter()
        .filter(|id| !canceled.contains(id) && !not_canceled.get(*id).is_some_and(|r| already_gone(r)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod verify_tests {
    use super::*;
    use polymarket_client_sdk_v2::error::{Error, Kind, StatusCode};

    #[test]
    fn only_a_404_counts_as_not_found() {
        let status = |code| Error::status(code, reqwest::Method::GET, "/data/order/x".into(), "msg");
        assert!(is_not_found(&status(StatusCode::NOT_FOUND)));
        for code in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::UNAUTHORIZED,
        ] {
            assert!(!is_not_found(&status(code)), "{code}");
        }
        let timeout =
            Error::with_source(Kind::Internal, std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"));
        assert!(!is_not_found(&timeout), "a timeout is not proof the order is gone");
    }

    #[test]
    fn only_matched_or_canceled_is_off_the_book() {
        assert!(is_off_book(&OrderStatusType::Matched) && is_off_book(&OrderStatusType::Canceled));
        for s in [
            OrderStatusType::Live,
            OrderStatusType::Delayed,
            OrderStatusType::Unmatched,
            OrderStatusType::Unknown("SOMETHING_NEW".into()),
        ] {
            assert!(!is_off_book(&s), "{s:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call to Polymarket that never answers ends at its limit, with an
    /// error the placer treats as an outage (not a rejection).
    #[tokio::test(start_paused = true)]
    async fn a_stalled_call_times_out_as_an_outage() {
        let stalled = std::future::pending::<std::result::Result<(), eyre::Report>>();
        let err = bounded("the heartbeat", HEARTBEAT_LIMIT, stalled).await.unwrap_err().to_string();
        assert_eq!(err, "the heartbeat timed out after 5s");
        assert!(crate::engine::placer::is_transient(&err));
        // A quick answer passes through, errors included.
        assert_eq!(bounded("x", READ_LIMIT, async { Ok::<_, eyre::Report>(7) }).await.unwrap(), 7);
        assert!(bounded("x", READ_LIMIT, async { Err::<(), _>(eyre::eyre!("nope")) }).await.is_err());
    }

    #[test]
    fn a_rejected_reply_is_an_error_not_a_placed_order() {
        assert_eq!(accepted_order_id(true, "0xabc", None).unwrap(), "0xabc");
        let msg = |r: Result<String>| format!("{:#}", r.unwrap_err());
        // HTTP 200 but success=false: an error, with Polymarket's reason.
        let e = msg(accepted_order_id(false, "", Some("not enough balance / allowance")));
        assert!(e.contains("not enough balance"), "{e}");
        assert!(crate::engine::placer::is_low_balance(&e), "reaches the No balance handling");
        // success=true but no id: still not something we can track or cancel.
        assert!(msg(accepted_order_id(true, "  ", None)).contains("no order id"));
        assert!(msg(accepted_order_id(false, "0xabc", Some(""))).contains("rejected"));
    }

    /// A market whose open orders can't be listed fails the startup check:
    /// carrying on as if it had none would trade beside its old orders.
    #[test]
    fn a_market_that_cannot_be_listed_fails_the_startup_check() {
        let ok = |v: &[&str]| Ok(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let (ids, failed) = gather_listings(vec![ok(&["a", "b"]), Err(eyre::eyre!("502")), ok(&["c"])]);
        assert_eq!(ids, vec!["a", "b", "c"], "what was found is still cancelled");
        assert_eq!(failed, 1);
        assert_eq!(gather_listings(vec![ok(&[]), ok(&["x"])]), (vec!["x".to_string()], 0));
    }

    #[test]
    fn order_pages_end_on_the_terminal_or_an_empty_cursor_and_are_capped() {
        assert_eq!(next_page("LTE=", 1).unwrap(), None);
        assert_eq!(next_page("", 1).unwrap(), None, "an empty cursor ends instead of restarting at page 1");
        assert_eq!(next_page("MTAw", 1).unwrap().as_deref(), Some("MTAw"));
        assert!(next_page("MTAw", MAX_ORDER_PAGES).is_err(), "never loops forever");
    }

    #[test]
    fn only_confirmed_cancels_count_as_gone() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let requested = ids(&["a", "b", "c", "d", "e"]);
        let canceled = ids(&["a"]);
        let not_canceled: std::collections::HashMap<String, String> = [
            ("b".to_string(), "order already canceled".to_string()),
            ("c".to_string(), "Order Already Matched".to_string()),
            ("d".to_string(), "rate limited".to_string()),
        ]
        .into();
        // e is missing from the answer: not known to be gone.
        assert_eq!(still_open(&requested, &canceled, &not_canceled), ids(&["d", "e"]));
        assert!(still_open(&[], &canceled, &not_canceled).is_empty());
    }
    use rust_decimal_macros::dec;

    fn entry(cid: u8, pct: &str, earned: &str) -> UserRewardsEarningResponse {
        let zero = format!("0x{}", "0".repeat(40));
        serde_json::from_value(serde_json::json!({
            "condition_id": format!("0x{cid:064x}"),
            "question": "Q", "market_slug": "m", "event_slug": "e", "image": "",
            "rewards_max_spread": "3", "rewards_min_size": "50", "market_competitiveness": "1",
            "maker_address": zero, "earning_percentage": pct,
            "earnings": [{ "asset_address": zero, "earnings": earned, "asset_rate": "1" }],
        }))
        .unwrap()
    }

    #[test]
    fn paused_market_with_earnings_is_kept_and_markets_are_not_doubled() {
        // Paused: no share any more, but it paid earlier today.
        let paused = entry(1, "0", "0.40");
        // Farming now and already paid: comes back from both walks.
        let active = entry(2, "12.5", "0.10");
        // Just started: a share, nothing paid yet.
        let fresh = entry(3, "3", "0");

        assert!(has_earnings(&paused) && !has_share(&paused));
        assert!(has_share(&fresh) && !has_earnings(&fresh));

        let merged = merge_reward_markets(vec![paused.clone(), active.clone()], vec![active, fresh]);
        let ids: Vec<_> = merged.iter().map(|e| e.condition_id).collect();
        assert_eq!(
            ids,
            vec![entry(1, "0", "0").condition_id, entry(2, "0", "0").condition_id, entry(3, "0", "0").condition_id]
        );
        let total: Decimal = merged.iter().flat_map(|e| &e.earnings).map(|a| a.earnings).sum();
        assert_eq!(total, dec!(0.50));
    }
}
