//! Live-chain integration test validating the L1 data-fee snapshot against
//! the Base mainnet GasPriceOracle predeploy. Regression tests for:
//!
//! 1. The original bug where three of the four getter reads
//!    (`l1BlobBaseFee`, `l1BaseFeeScalar`, `l1BlobBaseFeeScalar`) revert on
//!    Base mainnet, forcing `fetch_scan_snapshot` to always emit
//!    `l1_fee = None` and skip every scan.
//! 2. The review finding where feeding repeated `0xFF` bytes to
//!    `getL1Fee(bytes)` *under*prices the L1 fee: under Fjord's FastLZ
//!    compression the repeated bytes collapse, so that probe is not a
//!    conservative bound. `getL1FeeUpperBound` must be used instead and
//!    must price at least as high as a representative real transaction.
//!
//! `#[ignore]` by default; run with:
//!
//! ```sh
//! cargo test --test chain_l1fee -- --ignored
//! ```

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::ProviderBuilder;
use morpho_arbitrage_bot::dex::{fetch_scan_snapshot, unsigned_tx_rlp_len};
use std::str::FromStr;

const ORACLE: &str = "0x420000000000000000000000000000000000000F";
const CONTRACT: &str = "0x009a31ef076f2D9A3Ad9580dd16707926E027805";

alloy::sol! {
    #[sol(rpc)]
    interface IGasPriceOracle {
        function getL1Fee(bytes calldata _data) external view returns (uint256);
        function getL1FeeUpperBound(uint256 _unsignedTxSize) external view returns (uint256);
    }
}

/// RLP-encode a minimal EIP-1559 unsigned transaction whose `data` is
/// `calldata`; used to price a *representative real* tx with `getL1Fee`.
fn rlp_int(v: u64) -> Vec<u8> {
    if v == 0 {
        return vec![0x80];
    }
    let b = v.to_be_bytes();
    let start = b.iter().position(|&x| x != 0).unwrap_or(8);
    let out = &b[start..];
    if out.len() == 1 && out[0] < 0x80 {
        out.to_vec()
    } else {
        let mut r = vec![0x80 + out.len() as u8];
        r.extend_from_slice(out);
        r
    }
}

fn rlp_bytes(b: &[u8]) -> Vec<u8> {
    if b.len() == 1 && b[0] < 0x80 {
        return b.to_vec();
    }
    let mut r = vec![0x80 + b.len() as u8];
    r.extend_from_slice(b);
    r
}

fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.iter().flatten().copied().collect();
    let mut r = vec![0xc0 + payload.len() as u8];
    r.extend_from_slice(&payload);
    r
}

fn eip1559_unsigned_tx(calldata: &[u8]) -> Vec<u8> {
    let items = [
        rlp_int(8453),                                            // chainId (Base)
        rlp_int(0),                                               // nonce
        rlp_int(1_000_000),                                       // maxPriorityFeePerGas
        rlp_int(50_000_000),                                      // maxFeePerGas
        rlp_int(400_000),                                         // gasLimit
        rlp_bytes(Address::from_str(CONTRACT).unwrap().as_ref()), // to
        rlp_int(0),                                               // value
        rlp_bytes(calldata),                                      // data
        rlp_list(&[]),                                            // accessList
    ];
    let mut tx = vec![0x02]; // EIP-1559 enveloped
    tx.extend_from_slice(&rlp_list(&items));
    tx
}

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
        .expect("L1 fee snapshot must be priced on Base (getL1FeeUpperBound must not revert)");
    assert!(
        l1.l1_fee_wei > U256::ZERO,
        "priced L1 fee should be non-zero (l1_fee_wei={})",
        l1.l1_fee_wei,
    );
}

#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn upper_bound_prices_above_compressible_and_representative_txs() {
    let rpc =
        std::env::var("BASE_RPC_HTTP").unwrap_or_else(|_| "https://mainnet.base.org".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc.parse().unwrap());
    let oracle = IGasPriceOracle::new(Address::from_str(ORACLE).unwrap(), &provider);

    let calldata_len = 4 + 24 * 32; // exact bot execute calldata size

    // The review bug: repeated 0xFF is FastLZ-compressible, so getL1Fee on
    // that probe is NOT a conservative bound.
    let compressible_fee = oracle
        .getL1Fee(Bytes::from(vec![0xFF; calldata_len]))
        .call()
        .await
        .expect("getL1Fee(0xFF..) succeeds");

    // A representative real execute tx: random calldata (non-compressible),
    // full unsigned EIP-1559 envelope.
    // Build deterministic pseudo-random calldata without external deps.
    let mut calldata = vec![0u8; calldata_len];
    let mut x = 0x1234_5678_9abc_def0u64;
    for b in calldata.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
    let real_tx = eip1559_unsigned_tx(&calldata);
    let real_fee = oracle
        .getL1Fee(Bytes::from(real_tx))
        .call()
        .await
        .expect("getL1Fee(real tx) succeeds");

    // The bound uses the full unsigned tx size (envelope included), not the
    // bare calldata length.
    let bound_fee = oracle
        .getL1FeeUpperBound(U256::from(unsigned_tx_rlp_len(calldata_len)))
        .call()
        .await
        .expect("getL1FeeUpperBound succeeds");

    assert!(
        bound_fee > compressible_fee,
        "upper bound ({bound_fee}) must exceed the compressible 0xFF fee ({compressible_fee})",
    );
    assert!(
        bound_fee >= real_fee,
        "upper bound ({bound_fee}) must cover a representative real tx fee ({real_fee})",
    );
}
