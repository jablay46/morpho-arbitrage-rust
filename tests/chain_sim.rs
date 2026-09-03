//! Live-chain validation of the local revm simulation: `simulate_call`
//! (revm + lazily-fetched chain state) must return byte-identical output to
//! the node's own `eth_call` for the same calldata at the same pinned block.
//! `#[ignore]` by default -- it hits a public RPC -- so run it explicitly with:
//!
//! ```sh
//! cargo test --test chain_sim -- --ignored
//! ```

use alloy::primitives::{Address, Bytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol_types::SolCall;
use morpho_arbitrage_bot::sim::{simulate_call, SimOutcome};
use std::str::FromStr;

const SLIPSTREAM_FACTORY: &str = "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A";
const WETH: &str = "0x4200000000000000000000000000000000000006";
const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

alloy::sol! {
    #[sol(rpc)]
    interface IGetPool {
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address pool);
    }

    #[sol(rpc)]
    interface ISlot0 {
        function slot0() external view returns (uint160, int24, uint16, uint16, uint16, bool);
    }
}

async fn assert_local_matches_node<P: Provider + Clone>(
    provider: P,
    to: Address,
    calldata: Vec<u8>,
    block: u64,
) -> Vec<u8> {
    let node_out = provider
        .call(
            TransactionRequest::default()
                .to(to)
                .input(calldata.clone().into()),
        )
        .block(alloy::eips::BlockId::number(block))
        .await
        .expect("eth_call succeeds")
        .to_vec();

    let outcome = simulate_call(
        provider,
        to,
        Address::ZERO,
        Bytes::from(calldata),
        revm::database::BlockId::number(block),
    )
    .expect("local sim executes");
    match outcome {
        SimOutcome::Success { gas_used, output } => {
            assert!(gas_used > 0, "call consumes gas");
            assert_eq!(
                output.to_vec(),
                node_out,
                "local revm output must match eth_call byte-for-byte"
            );
        }
        SimOutcome::Reverted(reason) => panic!("local sim reverted where node succeeded: {reason}"),
    }
    node_out
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn local_sim_matches_eth_call() {
    let rpc =
        std::env::var("BASE_RPC_HTTP").unwrap_or_else(|_| "https://mainnet.base.org".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc.parse().unwrap());
    let block = provider.get_block_number().await.unwrap();

    // Case 1: plain storage-read call (immutable + code + SLOAD).
    let factory = Address::from_str(SLIPSTREAM_FACTORY).unwrap();
    let get_pool = IGetPool::getPoolCall {
        tokenA: Address::from_str(WETH).unwrap(),
        tokenB: Address::from_str(USDC).unwrap(),
        tickSpacing: alloy::primitives::aliases::I24::unchecked_from(100i32),
    }
    .abi_encode();
    let out = assert_local_matches_node(provider.clone(), factory, get_pool, block).await;
    assert_eq!(out.len(), 32);
    let pool = Address::from_slice(&out[12..]);
    assert_ne!(pool, Address::ZERO, "WETH/USDC ts=100 pool must exist");

    // Case 2: packed-struct getter on the resolved pool (packed SLOADs).
    let slot0 = ISlot0::slot0Call {}.abi_encode();
    assert_local_matches_node(provider, pool, slot0, block).await;
}
