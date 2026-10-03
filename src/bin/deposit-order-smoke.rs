/// Live test for the DEPOSIT-WALLET (EIP-1271 / POLY_1271) order path added in
/// SDK 0.7.0. Places a tiny far-from-market BUY with the deposit wallet as
/// funder, signed via the SDK's ERC-7739 wrapping, then cancels it.
///
/// Run with:
///   POLYMARKET_PRIVATE_KEY=0x… POLYMARKET_PROXY_WALLET=0x2E3C… \
///     cargo run --bin deposit-order-smoke [TOKEN_ID]
///
/// Interpreting the result:
///   "Order placed"                         → deposit-wallet signing fully works
///   error mentions balance/allowance/funds → signing WORKS (reached the balance
///                                             check past auth+signature)
///   "maker address not allowed" / "signer  → signing still broken
///     address has to be the address of…"
use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as _;
use eyre::Result;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::clob::types::Side;
use polymarket_client_sdk_v2::clob::types::SignatureType;
use polymarket_client_sdk_v2::clob::{Client, Config as ClobConfig};
use rust_decimal_macros::dec;
use std::str::FromStr;

const CLOB_URL: &str = "https://clob.polymarket.com";
const POLYGON_CHAIN_ID: u64 = 137;

type AuthClient = Client<Authenticated<Normal>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("deposit_order_smoke=debug,info").init();
    dotenvy::dotenv().ok();

    let private_key = std::env::var("POLYMARKET_PRIVATE_KEY").expect("POLYMARKET_PRIVATE_KEY not set");
    let proxy_wallet =
        Address::from_str(&std::env::var("POLYMARKET_PROXY_WALLET").expect("POLYMARKET_PROXY_WALLET not set"))
            .expect("invalid POLYMARKET_PROXY_WALLET");

    // Default token = the market from the user's recent run (active). Override via argv[1].
    let token_id = std::env::args().nth(1).unwrap_or_else(|| {
        "112157595552502425360311365480557346272967836691353340001604809204498706086576".to_string()
    });

    let signer: PrivateKeySigner = private_key.parse()?;
    let signer = signer.with_chain_id(Some(POLYGON_CHAIN_ID));
    println!("Signer (EOA):   {}", signer.address());
    println!("Funder (maker): {proxy_wallet}   [signatureType = Poly1271]");

    let client: AuthClient = Client::new(CLOB_URL, ClobConfig::default())?
        .authentication_builder(&signer)
        .funder(proxy_wallet)
        .signature_type(SignatureType::Poly1271)
        .authenticate()
        .await?;
    println!("Authenticated (deposit-wallet flow)");

    // Far below market so it rests without filling (best_bid was ~0.80).
    let price = dec!(0.05);
    let size = dec!(5);
    println!("\nPlacing BUY: token={}… @ {price} size {size}", &token_id[..12]);

    let order = client
        .limit_order()
        .token_id(U256::from_str(&token_id)?)
        .price(price)
        .size(size)
        .side(Side::Buy)
        .build()
        .await?;

    let signed = client.sign(&signer, order).await?;
    match client.post_order(signed).await {
        Ok(response) => {
            println!("\n✅ Order placed: {}", response.order_id);
            println!("Status: {:?}", response.status);
            println!("\n--- cancelling ---");
            let c = client.cancel_orders(&[response.order_id.as_str()]).await?;
            println!("canceled: {:?}  not_canceled: {:?}", c.canceled, c.not_canceled);
        }
        Err(e) => {
            let msg = e.to_string();
            println!("\n post_order returned: {msg}");
            let low = msg.to_lowercase();
            if low.contains("balance")
                || low.contains("allowance")
                || low.contains("funds")
                || low.contains("not enough")
            {
                println!("\n✅ SIGNING WORKS — reached the balance check (order was signature-valid).");
                println!("   (Fund the deposit wallet with pUSD to actually rest an order.)");
            } else if low.contains("maker address not allowed") || low.contains("api key") {
                println!("\n❌ Signing still rejected — deposit-wallet flow not accepted.");
            } else {
                println!("\n⚠️  Unrecognized response — inspect above.");
            }
        }
    }
    Ok(())
}
