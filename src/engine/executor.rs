use crate::config::Config;
use alloy::primitives::U256;
use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use eyre::Result;
use std::str::FromStr;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::clob::types::SignatureType;
use polymarket_client_sdk_v2::clob::{Client, Config as ClobConfig};
use rust_decimal::Decimal;
use secrecy::ExposeSecret;
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
    pub async fn new(config: &Config) -> Result<Self> {
        let signer: PrivateKeySigner = config.private_key.expose_secret().parse()?;
        let signer = signer.with_chain_id(Some(POLYGON_CHAIN_ID));

        info!("Signer address: {}", signer.address());

        let client = Client::new(CLOB_URL, ClobConfig::default())?
            .authentication_builder(&signer)
            .funder(config.proxy_wallet)
            .signature_type(SignatureType::GnosisSafe)
            .authenticate()
            .await?;

        info!("Authenticated with Polymarket CLOB");

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
    pub async fn place_buy_order(
        &self,
        token_id: &str,
        price: Decimal,
        size: Decimal,
    ) -> Result<String> {
        use polymarket_client_sdk_v2::clob::types::Side;

        // SDK V2 builders take a typed U256 token id; our token_id is a decimal string.
        let token_id_u256 = U256::from_str(token_id)
            .map_err(|e| eyre::eyre!("invalid token_id {token_id}: {e}"))?;

        let order = self.client
            .limit_order()
            .token_id(token_id_u256)
            .price(price)
            .size(size)
            .side(Side::Buy)
            .build()
            .await?;

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
                    let cancelled = response.canceled.iter()
                        .any(|id| id.to_lowercase() == id_lower);
                    if cancelled {
                        return Ok(true);
                    }

                    // Look for the order ID as the key (normal Polymarket response).
                    let keyed_reason = response.not_canceled.get(order_id)
                        .or_else(|| response.not_canceled.iter()
                            .find(|(k, _)| k.to_lowercase() == id_lower)
                            .map(|(_, v)| v));

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
                    warn!("Order {} status unclear — canceled={:?} not_canceled={:?}",
                        order_id, response.canceled, response.not_canceled);
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
        let asset_id = U256::from_str(token_id)
            .map_err(|e| eyre::eyre!("invalid token_id {token_id}: {e}"))?;
        let req = OrdersRequest::builder()
            .asset_id(asset_id)
            .build();
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
                Err(e) => error!("Failed to fetch orders for token {}...: {}", token_id.get(..8).unwrap_or(token_id), e),
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
}
