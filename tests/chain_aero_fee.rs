//! Live-chain integration test for the Aerodrome fee validation added to
//! `VenueCache::build`. `#[ignore]` by default — it hits a public RPC — so
//! run it explicitly with:
//!
//! ```sh
//! cargo test --test chain_aero_fee -- --ignored --test-threads=1
//! ```
//!
//! Two regressions are pinned here:
//!
//! 1. `AerodromeFactory.getPool(tokenA, tokenB, stable)` ignores the fee, so
//!    a config `fee_bps` that disagrees with the pool's real fee silently
//!    mispriced every quote. The factory's `volatileFee()` default (30) is
//!    NOT the per-pool fee — WETH/VIRTUAL on Base carries 100 bps, which the
//!    bot previously quoted at 30.
//! 2. The canonical factory answers `getFee` for *any* address, returning the
//!    30 bps default for pools it did not create. A bare `getFee` reading is
//!    therefore meaningless unless the factory is proved to own the pool, so
//!    ownership is checked first and any failure aborts startup rather than
//!    falling back to the configured fee.

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use morpho_arbitrage_bot::dex::fetch_aerodrome_fee_bps;
use std::str::FromStr;

const AERODROME_FACTORY: &str = "0x420DD381b31aEf6683db6B902084cB0FFECe40Da";
const AERODROME_ROUTER: &str = "0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874e43";
/// WETH/VIRTUAL classic volatile pool — the one configured at 100 bps.
const WETH_VIRTUAL_POOL: &str = "0x21594b992f68495dd28d605834b58889d0a727c7";
/// Uniswap V2 factory — a real factory, but not an Aerodrome one.
const UNIV2_FACTORY: &str = "0x8909Dc15e40173Ff4699343b6eB8132c65e18eC6";

fn provider() -> impl Provider + Clone {
    let rpc =
        std::env::var("BASE_RPC_HTTP").unwrap_or_else(|_| "https://mainnet.base.org".to_string());
    ProviderBuilder::new().connect_http(rpc.parse().unwrap())
}

fn addr(s: &str) -> Address {
    Address::from_str(s).unwrap()
}

#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn aerodrome_fee_reads_the_per_pool_rate_not_the_factory_default() {
    let provider = provider();
    let factory = addr(AERODROME_FACTORY);
    let router = addr(AERODROME_ROUTER);
    let pool = addr(WETH_VIRTUAL_POOL);

    // The factory default is 30 bps; the pool itself charges 100.
    let fee = fetch_aerodrome_fee_bps(&provider, factory, router, pool, false)
        .await
        .expect("getFee must succeed against the canonical factory");
    assert_eq!(
        fee, 100,
        "WETH/VIRTUAL volatile is a 100 bps pool; a 30 would mean we read the factory default"
    );

    // A zero factory must resolve through the router, yielding the same rate.
    let via_router = fetch_aerodrome_fee_bps(&provider, Address::ZERO, router, pool, false)
        .await
        .expect("router defaultFactory resolution must succeed");
    assert_eq!(via_router, fee);
}

/// The canonical factory happily answers `getFee` for a pool it did not
/// create (returning `volatileFee`, 30 bps). Trusting that reading would let
/// a wrong `pair`/`factory` in the config validate a fee that belongs to no
/// pool, so ownership is checked first and the mismatch is a hard error.
#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn pool_the_factory_does_not_own_is_rejected() {
    let provider = provider();
    let factory = addr(AERODROME_FACTORY);
    let router = addr(AERODROME_ROUTER);
    // An address the factory did not create. `getFee` would still return 30.
    let bogus = addr("0x1111111111111111111111111111111111111111");

    let err = fetch_aerodrome_fee_bps(&provider, factory, router, bogus, false)
        .await
        .expect_err("a pool this factory does not own must not validate a fee");
    let msg = err.to_string();
    assert!(
        msg.contains("does not own pool"),
        "expected an ownership error, got: {msg}"
    );
}

/// A real factory of the wrong kind (Uniswap V2) must abort startup rather
/// than validate a fee for a pool it does not own.
#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn wrong_kind_factory_is_rejected() {
    let provider = provider();
    let router = addr(AERODROME_ROUTER);
    let pool = addr(WETH_VIRTUAL_POOL);

    let result = fetch_aerodrome_fee_bps(&provider, addr(UNIV2_FACTORY), router, pool, false).await;
    assert!(
        result.is_err(),
        "a factory that cannot confirm ownership must abort startup, got {result:?}"
    );
}

/// The canonical factory's router is a contract, but it does not implement
/// `isPool`. A router in the factory slot is a misconfiguration, so it must
/// abort rather than validate the configured fee.
#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn router_passed_as_factory_is_rejected() {
    let provider = provider();
    let router = addr(AERODROME_ROUTER);
    let pool = addr(WETH_VIRTUAL_POOL);

    let result = fetch_aerodrome_fee_bps(&provider, router, router, pool, false).await;
    assert!(
        result.is_err(),
        "a router in the factory slot must abort startup, got {result:?}"
    );
}

/// The regression this whole change is about: an RPC failure must NOT be
/// interpreted as "this factory has no getter, keep the configured fee",
/// because that is what let a transient provider error validate the
/// configured fee and reach both Aerodrome quote directions with a stale
/// value. A dead provider has to abort startup instead. No network needed —
/// the connection itself fails.
#[tokio::test]
async fn provider_failure_aborts_startup() {
    let provider = ProviderBuilder::new().connect_http("http://127.0.0.1:1".parse().unwrap());
    let router = addr(AERODROME_ROUTER);
    let pool = addr(WETH_VIRTUAL_POOL);
    let factory = addr(AERODROME_FACTORY);

    let result = fetch_aerodrome_fee_bps(&provider, factory, router, pool, false).await;
    assert!(
        result.is_err(),
        "an unreachable provider must abort startup, not fall back to the configured fee"
    );
}
