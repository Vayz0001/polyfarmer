//! On-chain detection of a user's real Polymarket wallet address from their EOA.
//!
//! Polymarket wallets come in (at least) three independent flavors, each from
//! a different factory contract: classic Gnosis Safe and classic EIP-1167
//! Proxy (pre June 2026), and the newer "Deposit Wallet" (rolled out June 8
//! 2026 — `https://docs.polymarket.com/trading/deposit-wallets`), which itself
//! has two clone shapes (legacy UUPS / current BeaconProxy).
//!
//! Rather than reimplement Polymarket's CREATE2/Solady bytecode-hash math
//! (verified unreliable — see [`crate::creds`] module docs / memory notes:
//! our SDK's hardcoded constants did not match a real on-chain wallet), we
//! ask the chain directly:
//!   - Deposit Wallet candidates: call the factory's own
//!     `predictWalletAddress(address,bytes32)` view function — ground truth,
//!     no bytecode hashing needed on our end.
//!   - Classic Safe/Proxy candidates: still computed locally via the SDK's
//!     `derive_safe_wallet`/`derive_proxy_wallet` (the *formula* is confirmed
//!     correct against Polymarket's own example repos; only some constants
//!     drift over time).
//!
//! Every candidate is then checked with `eth_getCode` — only addresses with
//! real deployed bytecode are reported. This makes the result self-correcting
//! even if our local derivation constants go stale again: we only ever
//! surface candidates that actually exist on-chain.

use alloy::primitives::{keccak256, Address};
use eyre::{eyre, Result};
use polymarket_client_sdk_v2::{derive_proxy_wallet, derive_safe_wallet, POLYGON};
use serde_json::{json, Value};
use std::str::FromStr;
use std::time::Duration;

/// Default public Polygon RPC endpoints, tried in order until one responds.
/// Overridable/extendable via `POLYGON_RPC_URL` (tried first if set).
pub const DEFAULT_RPC_URLS: &[&str] = &[
    "https://polygon-bor-rpc.publicnode.com",
    "https://polygon.drpc.org",
    "https://polygon.meowrpc.com",
];

const DEPOSIT_WALLET_FACTORY: &str = "0x00000000000Fb5C9ADea0298D729A0CB3823Cc07";
const DEPOSIT_WALLET_BEACON: &str = "0x7A18EDfe055488A3128f01F563e5B479D92ffc3a";
const DEPOSIT_WALLET_LEGACY_IMPL: &str = "0x58ca52ebe0dadfdf531cde7062e76746de4db1eb";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletKind {
    DepositWallet,
    GnosisSafe,
    ProxyWallet,
}

impl WalletKind {
    pub fn label(&self) -> &'static str {
        match self {
            WalletKind::DepositWallet => "Deposit Wallet",
            WalletKind::GnosisSafe => "Gnosis Safe (browser wallet)",
            WalletKind::ProxyWallet => "Proxy wallet (email/Magic)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WalletCandidate {
    pub address: Address,
    pub kind: WalletKind,
}

/// `bytes32(owner)` — the address left-padded to 32 bytes, exactly as
/// Solidity's `abi.encode` represents an `address` parameter.
fn left_pad_32(addr: Address) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(addr.as_slice());
    out
}

/// 4-byte selector for `predictWalletAddress(address,bytes32)`.
fn predict_wallet_address_selector() -> [u8; 4] {
    let hash = keccak256(b"predictWalletAddress(address,bytes32)");
    let mut sel = [0u8; 4];
    sel.copy_from_slice(&hash[..4]);
    sel
}

fn predict_wallet_calldata(implementation: Address, wallet_id: [u8; 32]) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 32 + 32);
    data.extend_from_slice(&predict_wallet_address_selector());
    data.extend_from_slice(&left_pad_32(implementation));
    data.extend_from_slice(&wallet_id);
    data
}

/// Decode a 32-byte ABI-encoded `address` return value (last 20 bytes).
fn decode_address_result(hex_result: &str) -> Result<Address> {
    let bytes = hex::decode(hex_result.trim_start_matches("0x"))
        .map_err(|e| eyre!("bad hex in eth_call result: {e}"))?;
    if bytes.len() < 20 {
        return Err(eyre!("eth_call result too short for an address"));
    }
    Ok(Address::from_slice(&bytes[bytes.len() - 20..]))
}

/// Per-request HTTP timeout. Kept short so an unreachable/slow RPC fails fast
/// rather than leaving the UI hanging.
const PER_REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
/// Hard ceiling on the whole detection pass (connect + all calls), regardless
/// of how many candidates or retries are involved — guarantees the caller
/// gets an answer (success or "couldn't check") in bounded time, including
/// when the user has no internet connection at all.
const TOTAL_BUDGET: Duration = Duration::from_secs(9);

// ── Minimal JSON-RPC client ───────────────────────────────────────────────────

struct Rpc {
    client: reqwest::Client,
    url: String,
}

impl Rpc {
    /// Probe every candidate URL (configured one plus the public fallback
    /// list) *concurrently* and use whichever answers `eth_chainId` with
    /// Polygon's chain id (137 / 0x89) first. Concurrent rather than
    /// sequential so a handful of dead/slow endpoints — or no network at
    /// all — resolve in one timeout window, not one-per-candidate.
    async fn connect(configured: Option<&str>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(PER_REQUEST_TIMEOUT)
            .build()?;

        let mut urls: Vec<String> = Vec::new();
        if let Some(u) = configured {
            urls.push(u.to_string());
        }
        urls.extend(DEFAULT_RPC_URLS.iter().map(|s| s.to_string()));

        let mut probes: futures_util::stream::FuturesUnordered<_> = urls
            .into_iter()
            .map(|url| {
                let client = client.clone();
                async move {
                    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]});
                    let res = client.post(&url).json(&body).send().await.ok()?;
                    let v: Value = res.json().await.ok()?;
                    (v.get("result").and_then(|r| r.as_str()) == Some("0x89")).then_some(url)
                }
            })
            .collect();

        while let Some(result) = futures_util::StreamExt::next(&mut probes).await {
            if let Some(url) = result {
                return Ok(Self { client, url });
            }
        }
        Err(eyre!("no configured/public Polygon RPC responded — check your internet connection"))
    }

    async fn call(&self, method: &str, params: Value) -> Result<String> {
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let res: Value = self.client.post(&self.url).json(&body).send().await?.json().await?;
        if let Some(err) = res.get("error") {
            return Err(eyre!("RPC error from {}: {}", self.url, err));
        }
        res.get("result")
            .and_then(|r| r.as_str())
            .map(str::to_string)
            .ok_or_else(|| eyre!("RPC response missing result field"))
    }

    async fn eth_call(&self, to: Address, data: &[u8]) -> Result<String> {
        self.call(
            "eth_call",
            json!([{"to": to.to_string(), "data": format!("0x{}", hex::encode(data))}, "latest"]),
        )
        .await
    }

    async fn get_code(&self, addr: Address) -> Result<bool> {
        let code = self
            .call("eth_getCode", json!([addr.to_string(), "latest"]))
            .await?;
        Ok(!code.trim_start_matches("0x").is_empty())
    }
}

/// ERC-20 `balanceOf(address)` selector.
const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];

/// pUSD (Polymarket's collateral token) balance of `owner` — the Polymarket
/// wallet's real on-chain balance, in USD (6 decimals). Tries each RPC in turn
/// (a public endpoint can answer `eth_chainId` yet fail real calls), so one
/// flaky node doesn't blank the dashboard.
pub async fn pusd_balance(owner: Address, configured_rpc: Option<&str>) -> Result<rust_decimal::Decimal> {
    let token = polymarket_client_sdk_v2::contract_config(POLYGON, false)
        .ok_or_else(|| eyre!("no contract config for Polygon"))?
        .collateral;
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&BALANCE_OF_SELECTOR);
    data.extend_from_slice(&left_pad_32(owner));

    let client = reqwest::Client::builder().timeout(PER_REQUEST_TIMEOUT).build()?;
    let urls = configured_rpc.into_iter().chain(DEFAULT_RPC_URLS.iter().copied());
    let mut last_err = eyre!("no RPC configured");
    for url in urls {
        let rpc = Rpc { client: client.clone(), url: url.to_string() };
        match rpc.eth_call(token, &data).await {
            Ok(hex) => match parse_token_amount(&hex, 6) {
                Ok(v) => return Ok(v),
                Err(e) => last_err = e,
            },
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Decode a 32-byte uint256 eth_call result into a decimal with `decimals`.
fn parse_token_amount(hex_result: &str, decimals: u32) -> Result<rust_decimal::Decimal> {
    let bytes = hex::decode(hex_result.trim_start_matches("0x")).map_err(|e| eyre!("bad hex: {e}"))?;
    if bytes.len() < 32 {
        return Err(eyre!("balanceOf result too short"));
    }
    // Balances fit comfortably in u128 (≈3.4e38 base units).
    let raw = u128::from_be_bytes(bytes[16..32].try_into().expect("16 bytes"));
    if bytes[..16].iter().any(|b| *b != 0) {
        return Err(eyre!("balance out of range"));
    }
    let raw = i128::try_from(raw).map_err(|_| eyre!("balance out of range"))?;
    rust_decimal::Decimal::try_from_i128_with_scale(raw, decimals).map_err(|e| eyre!("balance: {e}"))
}

/// Detect which Polymarket wallet(s) actually exist on-chain for this EOA.
/// `configured_rpc` is tried first if set, then [`DEFAULT_RPC_URLS`]. Bounded
/// by [`TOTAL_BUDGET`] — always returns within that window, including when
/// there is no network connectivity at all.
pub async fn detect_wallets(eoa: Address, configured_rpc: Option<&str>) -> Result<Vec<WalletCandidate>> {
    match tokio::time::timeout(TOTAL_BUDGET, detect_wallets_inner(eoa, configured_rpc)).await {
        Ok(result) => result,
        Err(_) => Err(eyre!("timed out checking on-chain — network may be slow or unreachable")),
    }
}

/// Best-effort check: is `addr` a Polymarket **Deposit Wallet**? Reads the
/// on-chain EIP-712 domain (`eip712Domain()`); a deposit wallet reports
/// `name = "DepositWallet"`. A deposit wallet must be traded via the EIP-1271
/// (`Poly1271`) flow, not the classic Gnosis-Safe signature.
///
/// Any RPC/parse failure returns `false` so the caller falls back to the
/// classic path — a wrong guess here only affects the signature type, which the
/// CLOB validates anyway.
pub async fn is_deposit_wallet(addr: Address, configured_rpc: Option<&str>) -> bool {
    // selector for eip712Domain()  (ERC-5267)
    const EIP712_DOMAIN_SELECTOR: [u8; 4] = [0x84, 0xb0, 0x19, 0x6e];
    // "DepositWallet" as lowercase hex — searched for in the ABI-encoded return,
    // avoiding a full tuple decode.
    const DEPOSIT_WALLET_HEX: &str = "4465706f73697457616c6c6574";

    let Ok(rpc) = Rpc::connect(configured_rpc).await else { return false };
    match rpc.eth_call(addr, &EIP712_DOMAIN_SELECTOR).await {
        Ok(hex) => hex.to_lowercase().contains(DEPOSIT_WALLET_HEX),
        Err(_) => false,
    }
}

async fn detect_wallets_inner(eoa: Address, configured_rpc: Option<&str>) -> Result<Vec<WalletCandidate>> {
    let rpc = Rpc::connect(configured_rpc).await?;
    let wallet_id = left_pad_32(eoa);

    let beacon = Address::from_str(DEPOSIT_WALLET_BEACON).expect("valid constant");
    let legacy_impl = Address::from_str(DEPOSIT_WALLET_LEGACY_IMPL).expect("valid constant");
    let factory = Address::from_str(DEPOSIT_WALLET_FACTORY).expect("valid constant");

    // Both Deposit Wallet shapes checked concurrently, not sequentially.
    let deposit_wallet_futs = [legacy_impl, beacon].into_iter().map(|implementation| {
        let rpc = &rpc;
        let data = predict_wallet_calldata(implementation, wallet_id);
        async move {
            let result = rpc.eth_call(factory, &data).await.ok()?;
            decode_address_result(&result).ok().map(|addr| (addr, WalletKind::DepositWallet))
        }
    });
    let mut raw_candidates: Vec<(Address, WalletKind)> =
        futures_util::future::join_all(deposit_wallet_futs).await.into_iter().flatten().collect();

    if let Some(addr) = derive_safe_wallet(eoa, POLYGON) {
        raw_candidates.push((addr, WalletKind::GnosisSafe));
    }
    if let Some(addr) = derive_proxy_wallet(eoa, POLYGON) {
        raw_candidates.push((addr, WalletKind::ProxyWallet));
    }

    // Deduplicate by address: the two Deposit Wallet clone shapes (legacy UUPS +
    // current Beacon) frequently predict the SAME address, and a derived
    // Safe/Proxy can coincide too. Keep the first (highest-priority) kind so the
    // picker never shows the same address twice.
    let mut seen = std::collections::HashSet::new();
    raw_candidates.retain(|(addr, _)| seen.insert(*addr));

    // Likewise, check all candidates' on-chain code concurrently.
    let code_futs = raw_candidates.iter().map(|&(address, kind)| {
        let rpc = &rpc;
        async move {
            rpc.get_code(address).await.unwrap_or(false).then_some(WalletCandidate { address, kind })
        }
    });
    let confirmed = futures_util::future::join_all(code_futs).await.into_iter().flatten().collect();
    Ok(confirmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_matches_known_value() {
        // keccak256("predictWalletAddress(address,bytes32)")[..4] — verified
        // against the live Deposit Wallet Factory contract on Polygon.
        assert_eq!(hex::encode(predict_wallet_address_selector()), "1f264778");
    }

    #[test]
    fn wallet_id_left_pads_address() {
        let addr = Address::from_str("0xe2d1DB006b8042c99AA8Ae31a0ada8D9b7f11c4E").unwrap();
        let id = left_pad_32(addr);
        assert_eq!(&id[..12], &[0u8; 12]);
        assert_eq!(&id[12..], addr.as_slice());
    }

    #[test]
    fn calldata_layout_matches_known_call() {
        // Reproduces the exact calldata verified against the live factory.
        let implementation =
            Address::from_str("0x7A18EDfe055488A3128f01F563e5B479D92ffc3a").unwrap();
        let eoa = Address::from_str("0xe2d1DB006b8042c99AA8Ae31a0ada8D9b7f11c4E").unwrap();
        let data = predict_wallet_calldata(implementation, left_pad_32(eoa));
        assert_eq!(
            hex::encode(&data),
            "1f2647780000000000000000000000007a18edfe055488a3128f01f563e5b479d92ffc3a\
             000000000000000000000000e2d1db006b8042c99aa8ae31a0ada8d9b7f11c4e"
        );
    }

    #[test]
    fn decode_address_result_takes_last_20_bytes() {
        let result = "0x00000000000000000000000078f3fbbad90d9076126e05cb4b834c074a84cfb0";
        let addr = decode_address_result(result).unwrap();
        assert_eq!(
            addr,
            Address::from_str("0x78f3fbbaD90D9076126E05Cb4b834C074a84CFb0").unwrap()
        );
    }

    #[test]
    fn parses_pusd_balance_in_usd() {
        // 100.123456 pUSD = 100_123_456 base units (6 decimals).
        let hex = format!("0x{:064x}", 100_123_456u128);
        assert_eq!(parse_token_amount(&hex, 6).unwrap().to_string(), "100.123456");
        assert_eq!(parse_token_amount(&format!("0x{:064x}", 0u8), 6).unwrap().to_string(), "0.000000");
        assert!(parse_token_amount("0x12", 6).is_err());
    }

    #[test]
    fn balance_of_calldata_is_selector_plus_padded_owner() {
        let owner = Address::from_str("0xe2d1DB006b8042c99AA8Ae31a0ada8D9b7f11c4E").unwrap();
        let mut data = BALANCE_OF_SELECTOR.to_vec();
        data.extend_from_slice(&left_pad_32(owner));
        assert_eq!(
            hex::encode(&data),
            "70a08231000000000000000000000000e2d1db006b8042c99aa8ae31a0ada8d9b7f11c4e"
        );
    }

    #[test]
    fn decode_address_result_rejects_short_input() {
        assert!(decode_address_result("0x1234").is_err());
    }
}
