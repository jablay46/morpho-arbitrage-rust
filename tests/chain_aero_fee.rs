//! Live-chain integration test for the Aerodrome fee validation added to
//! `VenueCache::build`. `#[ignore]` by default — it hits a public RPC — so
//! run it explicitly with:
//!
//! ```sh
//! cargo test --test chain_aero_fee -- --ignored
//! ```
//!
//! The regression this guards: `AerodromeFactory.getPool(tokenA, tokenB,
//! stable)` ignores the fee, so a config `fee_bps` that disagrees with the
//! pool's real fee silently mispriced every quote. The factory's
//! `volatileFee()` default (30) is NOT the per-pool fee — WETH/VIRTUAL on
//! Base carries 100 bps, which the bot previously quoted at 30.

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use morpho_arbitrage_bot::dex::fetch_aerodrome_fee_bps;
use std::str::FromStr;

const AERODROME_FACTORY: &str = "0x420DD381b31aEf6683db6B902084cB0FFECe40Da";
const AERODROME_ROUTER: &str = "0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874e43";
/// WETH/VIRTUAL classic volatile pool — the one configured at 100 bps.
const WETH_VIRTUAL_POOL: &str = "0x21594b992f68495dd28d605834b58889d0a727c7";

#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn aerodrome_fee_reads_the_per_pool_rate_not_the_factory_default() {
    let rpc =
        std::env::var("BASE_RPC_HTTP").unwrap_or_else(|_| "https://mainnet.base.org".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc.parse().unwrap());

    let factory = Address::from_str(AERODROME_FACTORY).unwrap();
    let router = Address::from_str(AERODROME_ROUTER).unwrap();
    let pool = Address::from_str(WETH_VIRTUAL_POOL).unwrap();

    // The factory default is 30 bps; the pool itself charges 100.
    let fee = fetch_aerodrome_fee_bps(&provider, factory, router, pool, false)
        .await
        .expect("getFee must succeed against the canonical factory")
        .expect("canonical factory implements getFee");
    assert_eq!(
        fee, 100,
        "WETH/VIRTUAL volatile is a 100 bps pool; a 30 would mean we read the factory default"
    );

    // A zero factory must resolve through the router, yielding the same rate.
    let via_router = fetch_aerodrome_fee_bps(&provider, Address::ZERO, router, pool, false)
        .await
        .expect("router defaultFactory resolution must succeed")
        .expect("canonical factory implements getFee");
    assert_eq!(via_router, fee);
}
