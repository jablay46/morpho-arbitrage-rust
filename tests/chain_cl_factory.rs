//! Live-chain integration test for the quoter↔factory cross-check. `#[ignore]`
//! by default — it hits a public RPC — so run it explicitly with:
//!
//! ```sh
//! cargo test --test chain_cl_factory -- --ignored
//! ```
//!
//! Background: Aerodrome runs two CL factories on Base (legacy
//! 0x5e7BB104...809A and successor 0xf8f2eB49...61Ef). Both deploy pools for
//! the same pair at the same tickSpacing and their QuoterV2s accept identical
//! calldata, so a mismatched (pool, quoter) pairing returns a plausible price
//! for the *other* deployment's pool instead of reverting. These tests pin the
//! guard that turns that silent mispricing into a startup failure.

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use morpho_arbitrage_bot::dex::verify_quoter_factory;
use std::str::FromStr;

const LEGACY_FACTORY: &str = "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A";
const LEGACY_QUOTER: &str = "0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0";
const NEW_FACTORY: &str = "0xf8f2eB4940CFE7d13603DDDD87f123820Fc061Ef";
const NEW_QUOTER: &str = "0x514c8B5f54112481E28028F1166Bd78501089259";
const UNIV3_FACTORY: &str = "0x33128a8fC17869897dcE68Ed026d694621f6FDfD";
const UNIV3_QUOTER: &str = "0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a";

fn provider() -> impl alloy::providers::Provider {
    let rpc =
        std::env::var("BASE_RPC_HTTP").unwrap_or_else(|_| "https://mainnet.base.org".to_string());
    ProviderBuilder::new().connect_http(rpc.parse().unwrap())
}

const ADDRS: [(&str, &str); 3] = [
    (LEGACY_FACTORY, LEGACY_QUOTER),
    (NEW_FACTORY, NEW_QUOTER),
    (UNIV3_FACTORY, UNIV3_QUOTER),
];

#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn matching_quoter_and_factory_are_accepted() {
    let provider = provider();
    for (factory, quoter) in ADDRS {
        verify_quoter_factory(
            &provider,
            Address::from_str(quoter).unwrap(),
            Address::from_str(factory).unwrap(),
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("{quoter} does price {factory}: {e}"));
    }
}

#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn legacy_quoter_with_new_factory_is_rejected() {
    let provider = provider();
    let err = verify_quoter_factory(
        &provider,
        Address::from_str(LEGACY_QUOTER).unwrap(),
        Address::from_str(NEW_FACTORY).unwrap(),
        3,
    )
    .await
    .expect_err("the legacy quoter does not price pools of the successor factory");
    let msg = err.to_string();
    assert!(
        msg.contains("prices factory") && msg.contains("venue 3"),
        "expected a quoter/factory mismatch naming the venue, got: {msg}"
    );
}

#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn new_quoter_with_legacy_factory_is_rejected() {
    let provider = provider();
    let err = verify_quoter_factory(
        &provider,
        Address::from_str(NEW_QUOTER).unwrap(),
        Address::from_str(LEGACY_FACTORY).unwrap(),
        1,
    )
    .await
    .expect_err("the successor quoter does not price pools of the legacy factory");
    assert!(err.to_string().contains("prices factory"), "got: {err}");
}

#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn zero_addresses_skip_the_check() {
    let provider = provider();
    // A venue with no explicit factory (e.g. resolved from the router) must
    // not block startup, and neither must a zero quoter.
    verify_quoter_factory(
        &provider,
        Address::from_str(NEW_QUOTER).unwrap(),
        Address::ZERO,
        0,
    )
    .await
    .expect("a zero factory has nothing to cross-check");
    verify_quoter_factory(
        &provider,
        Address::ZERO,
        Address::from_str(NEW_FACTORY).unwrap(),
        0,
    )
    .await
    .expect("a zero quoter has nothing to cross-check");
}
