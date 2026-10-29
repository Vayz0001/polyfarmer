use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as _;
use chrono::NaiveDate;
use eyre::Result;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::clob::types::request::UserRewardsEarningRequest;
use polymarket_client_sdk_v2::clob::types::response::UserRewardsEarningResponse;
use polymarket_client_sdk_v2::clob::types::SignatureType;
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
const RETRY_BASE_MS: u64 = 200;

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
        } else if crate::wallet_detect::is_deposit_wallet(proxy_wallet, rpc_url).await {
            SignatureType::Poly1271
        } else if Some(proxy_wallet) == derive_proxy_wallet(eoa, POLYGON) {
            SignatureType::Proxy
        } else {
            SignatureType::GnosisSafe
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
        let client = auth.authenticate().await?;

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

    /// Place a resting GTC BUY order. Returns the order ID.
    /// Price must already be snapped to tick size before calling.
    pub async fn place_buy_order(&self, token_id: &str, price: Decimal, size: Decimal) -> Result<String> {
        use polymarket_client_sdk_v2::clob::types::Side;

        // SDK V2 builders take a typed U256 token id; our token_id is a decimal string.
        let token_id_u256 = U256::from_str(token_id).map_err(|e| eyre::eyre!("invalid token_id {token_id}: {e}"))?;

        let order =
            self.client.limit_order().token_id(token_id_u256).price(price).size(size).side(Side::Buy).build().await?;

        let signed = self.client.sign(&self.signer, order).await?;
        let response = self.client.post_order(signed).await?;

        info!("Order placed: {} @ {} size {}", response.order_id, price, size);
        Ok(response.order_id)
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
            match self.client.cancel_orders(&[order_id]).await {
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
        Err(last_err.unwrap().into())
    }

    /// Cancel an order and verify the result via GET /order when the cancel
    /// response is ambiguous (i.e. cancel_order returned Ok(false)).
    ///
    /// Returns:
    ///   Ok(true)  — order confirmed off the CLOB (cancelled, filled, or never existed)
    ///   Ok(false) — order verified still active on the CLOB (cancel did not take effect)
    ///   Err(_)    — network/API failure; caller should leave state as Cancelling
    pub async fn cancel_order_verified(&self, order_id: &str) -> Result<bool> {
        match self.cancel_order(order_id).await? {
            true => Ok(true),
            false => {
                // Cancel response was ambiguous — verify actual CLOB state.
                // order() returns Err if the order doesn't exist (404/not found).
                info!("cancel_order ambiguous for {} — verifying via GET /order", order_id);
                match self.client.order(order_id).await {
                    Err(_) => {
                        // Order not found on CLOB = genuinely gone (filled or cancelled)
                        info!("Order {} confirmed gone via GET /order", order_id);
                        Ok(true)
                    }
                    Ok(_) => {
                        // Order exists per GET but cancel says "can't be found" — retry once.
                        warn!("Order {} still in GET /order — retrying cancel", order_id);
                        match self.cancel_order(order_id).await {
                            Ok(true) => Ok(true),
                            Ok(false) => {
                                // Both cancel attempts say "can't be found" while order record exists.
                                // This is the Polymarket matching-state race: the matching engine has
                                // consumed the order (hence "can't be found" to cancel) but the order
                                // record hasn't been cleaned up yet. Treat as gone — it is being filled.
                                warn!("Order {} both cancel attempts ambiguous — order is matching/settling, treating as gone", order_id);
                                Ok(true)
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
            }
        }
    }

    /// Cancel a specific list of order IDs (bot-managed orders only).
    /// Uses the bulk SDK endpoint — one API call for all IDs. Retries on transient errors.
    pub async fn cancel_orders(&self, order_ids: &[String]) -> Result<()> {
        if order_ids.is_empty() {
            info!("cancel_orders: nothing to cancel");
            return Ok(());
        }
        info!("cancel_orders: cancelling {} orders", order_ids.len());
        let ids_ref: Vec<&str> = order_ids.iter().map(|s| s.as_str()).collect();
        let mut last_err = None;
        for attempt in 1..=RETRY_ATTEMPTS {
            match self.client.cancel_orders(&ids_ref).await {
                Ok(response) => {
                    for id in &response.canceled {
                        info!("Cancelled order {}", id);
                    }
                    for (id, reason) in &response.not_canceled {
                        if reason.contains("already canceled") || reason.contains("already matched") {
                            info!("Order {} already gone ({})", id, reason);
                        } else {
                            warn!("Order {} NOT cancelled — reason: {}", id, reason);
                        }
                    }
                    return Ok(());
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
        Err(last_err.unwrap().into())
    }

    /// Fetch all open order IDs for a single token_id, handling pagination.
    /// Retries each page fetch on transient errors.
    async fn fetch_open_order_ids(&self, token_id: &str) -> Result<Vec<String>> {
        use polymarket_client_sdk_v2::clob::types::request::OrdersRequest;
        // SDK V2: asset_id is a typed U256; parse our decimal-string token id.
        let asset_id = U256::from_str(token_id).map_err(|e| eyre::eyre!("invalid token_id {token_id}: {e}"))?;
        let req = OrdersRequest::builder().asset_id(asset_id).build();
        let mut ids = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut last_err = None;
            let page = 'retry: {
                for attempt in 1..=RETRY_ATTEMPTS {
                    match self.client.orders(&req, cursor.clone()).await {
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
                return Err(eyre::eyre!("{}", last_err.unwrap()));
            };
            for order in &page.data {
                ids.push(order.id.clone());
            }
            if page.next_cursor == TERMINAL_CURSOR {
                break;
            }
            cursor = Some(page.next_cursor);
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
        let mut all_ids = Vec::new();
        for token_id in token_ids {
            match self.fetch_open_order_ids(token_id).await {
                Ok(ids) => {
                    info!("  token {}...: {} open orders", token_id.get(..8).unwrap_or(token_id), ids.len());
                    all_ids.extend(ids);
                }
                Err(e) => {
                    error!("Failed to fetch orders for token {}...: {}", token_id.get(..8).unwrap_or(token_id), e)
                }
            }
        }
        self.cancel_orders(&all_ids).await
    }

    /// Connectivity + auth health check.
    /// Calls GET /api-keys (L2-authenticated) every 5s.
    /// This verifies both network reachability and that our credentials are still valid.
    /// 3 consecutive failures trigger selective cancel of bot-managed orders as a safety net.
    /// Note: GTC orders do NOT expire without this call — it is purely a watchdog.
    pub async fn send_heartbeat(&self) -> Result<()> {
        self.client.api_keys().await?;
        Ok(())
    }

    // ── Reward data (read-only, display) ────────────────────────────────────
    // No retry/backoff here, unlike the order-management methods above — these
    // back dashboard reads, not money-moving actions. A transient failure just
    // propagates to the caller (the web handler shows "try again").

    /// Batch scoring check — one call for the whole markets table instead of
    /// one per row.
    pub async fn are_orders_scoring(&self, order_ids: &[&str]) -> Result<HashMap<String, bool>> {
        Ok(self.client.are_orders_scoring(order_ids).await?)
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
            let page = self.client.user_earnings_and_markets_config(&request, cursor.clone()).await?;
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
        let resp = self.client.balance_allowance(req).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
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
