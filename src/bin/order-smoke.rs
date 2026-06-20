/// Quick smoke test: place a tiny BUY order then cancel it by order ID.
///
/// Run with:
///   cargo run --bin order-smoke
///
/// Requires .env with POLYMARKET_PRIVATE_KEY and POLYMARKET_PROXY_WALLET.

use alloy::primitives::{Address, U256};
use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use eyre::Result;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::clob::types::SignatureType;
use polymarket_client_sdk_v2::clob::types::Side;
use polymarket_client_sdk_v2::clob::{Client, Config as ClobConfig};
use rust_decimal_macros::dec;
use std::str::FromStr;

const CLOB_URL: &str = "https://clob.polymarket.com";
const POLYGON_CHAIN_ID: u64 = 137;

type AuthClient = Client<Authenticated<Normal>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("order_smoke=debug,info")
        .init();

    dotenvy::dotenv().ok();

    let private_key = std::env::var("POLYMARKET_PRIVATE_KEY")
        .expect("POLYMARKET_PRIVATE_KEY not set");
    let proxy_wallet = Address::from_str(
        &std::env::var("POLYMARKET_PROXY_WALLET").expect("POLYMARKET_PROXY_WALLET not set")
    ).expect("invalid POLYMARKET_PROXY_WALLET");

    let signer: PrivateKeySigner = private_key.parse()?;
    let signer = signer.with_chain_id(Some(POLYGON_CHAIN_ID));
    println!("Signer: {}", signer.address());

    let client: AuthClient = Client::new(CLOB_URL, ClobConfig::default())?
        .authentication_builder(&signer)
        .funder(proxy_wallet)
        .signature_type(SignatureType::GnosisSafe)
        .authenticate()
        .await?;

    println!("Authenticated");

    // YES token for "US forces enter Iran by March 31?"
    let token_id = "42750054381142639205639663180818682570869285140532640407891991570656047928885";

    // Place a tiny order well below market (0.01) so it rests and doesn't fill
    let price = dec!(0.01);
    let size  = dec!(5); // 5 shares

    println!("\nPlacing BUY: token={}... @ {} size {}", &token_id[..8], price, size);

    let token_id_u256 = U256::from_str(token_id)?;

    let order = client
        .limit_order()
        .token_id(token_id_u256)
        .price(price)
        .size(size)
        .side(Side::Buy)
        .build()
        .await?;

    let signed   = client.sign(&signer, order).await?;
    let response = client.post_order(signed).await?;
    let order_id = response.order_id.clone();

    println!("Order placed: {}", order_id);
    println!("Status: {:?}", response.status);

    println!("\nWaiting 5s...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Try singular cancel
    println!("--- cancel_order (singular) ---");
    let cancel = client.cancel_order(&order_id).await?;
    println!("canceled:     {:?}", cancel.canceled);
    println!("not_canceled: {:?}", cancel.not_canceled);

    // Try bulk cancel regardless
    println!("\n--- cancel_orders (bulk, 1 id) ---");
    let cancel2 = client.cancel_orders(&[order_id.as_str()]).await?;
    println!("canceled:     {:?}", cancel2.canceled);
    println!("not_canceled: {:?}", cancel2.not_canceled);

    println!("\n--- GET /order after cancels ---");
    match client.order(&order_id).await {
        Ok(o)  => println!("Order still exists: id={} status={:?}", o.id, o.status),
        Err(e) => println!("GET /order error (likely gone): {}", e),
    }

    Ok(())
}
