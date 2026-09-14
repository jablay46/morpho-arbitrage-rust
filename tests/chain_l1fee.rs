//! Live-chain integration test validating the L1 data-fee snapshot against
//! the Base mainnet GasPriceOracle predeploy. Regression test for the bug
//! where three of the four getter reads (`l1BlobBaseFee`,
//! `l1BaseFeeScalar`, `l1BlobBaseFeeScalar`) revert on Base mainnet, which
//! forced `fetch_scan_snapshot` to always emit `l1_fee = None` and skip
//! every scan. The snapshot now reads `getL1Fee(bytes)` — the predeploy's
//! universal pricing entry point — which returns a priced fee on Base.
//! `#[ignore]` by default; run with:
//!
//! ```sh
//! cargo test --test chain_l1fee -- --ignored
//! ```

use alloy::eips::BlockId;
use alloy::primitives::U256;
use alloy::providers::ProviderBuilder;
use morpho_arbitrage_bot::dex::fetch_scan_snapshot;

#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn l1_fee_snapshot_is_priced_on_base_mainnet() {
    let rpc =
        std::env::var("BASE_RPC_HTTP").unwrap_or_else(|_| "https://mainnet.base.org".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc.parse().unwrap());

    // Empty venue/quote lists still exercise the full snapshot path: the
    // L1 oracle calls ride the same Multicall3 batch that failed when the
    // getter-based reads reverted on Base mainnet.
    let snapshot = fetch_scan_snapshot(&provider, &[], &[], &[], BlockId::latest(), 4 + 24 * 32)
        .await
        .expect("snapshot with oracle reads succeeds");

    let l1 = snapshot
        .l1_fee
        .expect("L1 fee snapshot must be priced on Base (getL1Fee must not revert)");
    assert!(
        l1.l1_fee_wei > U256::ZERO,
        "priced L1 fee should be non-zero (l1_fee_wei={})",
        l1.l1_fee_wei,
    );
}
