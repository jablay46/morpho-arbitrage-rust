//! Live-chain integration test for the Aerodrome fee validation added to
//! `VenueCache::build`. `#[ignore]` by default — it hits a public RPC — so
//! run it explicitly with:
//!
//! ```sh
//! cargo test --test chain_aero_fee -- --ignored --test-threads=1
//! ```
//!
//! Three regressions are pinned here:
//!
//! 1. `AerodromeFactory.getPool(tokenA, tokenB, stable)` ignores the fee, so
//!    a config `fee_bps` that disagrees with the pool's real fee silently
//!    mispriced every quote. The factory's `volatileFee()` default (30) is
//!    NOT the per-pool fee — WETH/VIRTUAL on Base carries 100 bps, which the
//!    bot previously quoted at 30.
//! 2. The canonical factory answers `getFee` for *any* address, returning the
//!    30 bps default for pools it did not create, so a bare `getFee` reading
//!    proves nothing unless the factory is known to own the pool.
//! 3. One factory owns *both* pools of a pair, so ownership alone cannot tell
//!    them apart: WETH/VIRTUAL has a volatile pool (100 bps) and a stable
//!    pool (5 bps). A config naming the stable pool with `stable = false`
//!    passed an ownership-only check while scanning and execution traded
//!    different pools. The factory's own `getPool` answer is what pins the
//!    pair and flag to the pool being validated.

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use morpho_arbitrage_bot::dex::fetch_aerodrome_fee_bps;
use std::str::FromStr;

const AERODROME_FACTORY: &str = "0x420DD381b31aEf6683db6B902084cB0FFECe40Da";
const AERODROME_ROUTER: &str = "0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874e43";
const WETH: &str = "0x4200000000000000000000000000000000000006";
const VIRTUAL: &str = "0x0b3e328455c4059EEb9e3f84b5543F74E24e7E1b";
/// WETH/VIRTUAL classic volatile pool — the one configured at 100 bps.
const WETH_VIRTUAL_POOL: &str = "0x21594b992f68495dd28d605834b58889d0a727c7";
/// WETH/VIRTUAL classic stable pool — a different pool, owned by the same
/// factory, charged 5 bps.
const WETH_VIRTUAL_STABLE_POOL: &str = "0x5358A89a2B61AE92F5C519018dD9Ae427Eb38026";
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
    let fee = fetch_aerodrome_fee_bps(
        &provider,
        factory,
        router,
        addr(WETH),
        addr(VIRTUAL),
        pool,
        false,
    )
    .await
    .expect("getFee must succeed against the canonical factory");
    assert_eq!(
        fee, 100,
        "WETH/VIRTUAL volatile is a 100 bps pool; a 30 would mean we read the factory default"
    );

    // A zero factory must resolve through the router, yielding the same rate.
    let via_router = fetch_aerodrome_fee_bps(
        &provider,
        Address::ZERO,
        router,
        addr(WETH),
        addr(VIRTUAL),
        pool,
        false,
    )
    .await
    .expect("router defaultFactory resolution must succeed");
    assert_eq!(via_router, fee);
}

/// The hole ownership checking alone leaves open. Both WETH/VIRTUAL pools
/// belong to the canonical factory, so `isPool` says yes to either — but they
/// charge 100 and 5 bps. Naming the stable pool while `stable = false` must be
/// refused, because the factory resolves `stable = false` to the volatile
/// pool: scanning and execution would otherwise trade different pools.
#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn owned_pool_from_the_opposite_stable_class_is_rejected() {
    let provider = provider();
    let factory = addr(AERODROME_FACTORY);
    let router = addr(AERODROME_ROUTER);

    let err = fetch_aerodrome_fee_bps(
        &provider,
        factory,
        router,
        addr(WETH),
        addr(VIRTUAL),
        addr(WETH_VIRTUAL_STABLE_POOL),
        false,
    )
    .await
    .expect_err("the stable pool must not validate under stable = false");
    let msg = err.to_string();
    assert!(
        msg.contains("but the config names pool"),
        "expected a pool-mismatch error, got: {msg}"
    );

    // The same pool is fine once the flag agrees with it, which is what makes
    // this a flag/config bug rather than a bad pool.
    let fee = fetch_aerodrome_fee_bps(
        &provider,
        factory,
        router,
        addr(WETH),
        addr(VIRTUAL),
        addr(WETH_VIRTUAL_STABLE_POOL),
        true,
    )
    .await
    .expect("the factory's own stable pool must validate");
    assert_eq!(fee, 5, "the WETH/VIRTUAL stable pool charges 5 bps");
}

/// The canonical factory happily answers `getFee` for a pool it did not
/// create (returning `volatileFee`, 30 bps). Trusting that reading would let
/// a wrong `pair`/`factory` in the config validate a fee that belongs to no
/// pool, so the resolution mismatch is caught before the fee is read.
#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn pool_the_factory_does_not_own_is_rejected() {
    let provider = provider();
    let factory = addr(AERODROME_FACTORY);
    let router = addr(AERODROME_ROUTER);
    // An address the factory did not create. `getFee` would still return 30.
    let bogus = addr("0x1111111111111111111111111111111111111111");

    let err = fetch_aerodrome_fee_bps(
        &provider,
        factory,
        router,
        addr(WETH),
        addr(VIRTUAL),
        bogus,
        false,
    )
    .await
    .expect_err("a pool this factory does not own must not validate a fee");
    let msg = err.to_string();
    // Assert on the mismatch text specifically. Accepting any error here would
    // let a throttled RPC (also an error) look like a passing test.
    assert!(
        msg.contains("but the config names pool"),
        "expected the resolution mismatch to name the configured pool, got: {msg}"
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

    let result = fetch_aerodrome_fee_bps(
        &provider,
        addr(UNIV2_FACTORY),
        router,
        addr(WETH),
        addr(VIRTUAL),
        pool,
        false,
    )
    .await;
    assert!(
        result.is_err(),
        "a factory that cannot resolve the pair must abort startup, got {result:?}"
    );
}

/// The canonical factory's router is a contract, but it does not implement
/// `getPool` or `isPool`. A router in the factory slot is a misconfiguration,
/// so it must abort rather than validate the configured fee.
#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn router_passed_as_factory_is_rejected() {
    let provider = provider();
    let router = addr(AERODROME_ROUTER);
    let pool = addr(WETH_VIRTUAL_POOL);

    let result = fetch_aerodrome_fee_bps(
        &provider,
        router,
        router,
        addr(WETH),
        addr(VIRTUAL),
        pool,
        false,
    )
    .await;
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

    let result = fetch_aerodrome_fee_bps(
        &provider,
        factory,
        router,
        addr(WETH),
        addr(VIRTUAL),
        pool,
        false,
    )
    .await;
    assert!(
        result.is_err(),
        "an unreachable provider must abort startup, not fall back to the configured fee"
    );
}
