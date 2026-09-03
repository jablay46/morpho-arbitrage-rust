//! Live-chain validation of local revm gas estimation against the node's
//! `eth_estimateGas` on the *same calldata and pinned block*. Uses real
//! deployed contracts (WETH9 + Slipstream factory) since no FlashArbitrage
//! deployment exists in this fork. `#[ignore]` by default; run with:
//!
//! ```sh
//! cargo test --test chain_gas -- --ignored
//! ```

use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol_types::SolCall;
use morpho_arbitrage_bot::sim::{fetch_sim_env, simulate_call, SimOutcome};
use std::str::FromStr;

const WETH: &str = "0x4200000000000000000000000000000000000006";
const BASE_BRIDGE: &str = "0x4200000000000000000000000000000000000010";

alloy::sol! {
    #[sol(rpc)]
    interface IWETH {
        function deposit() external payable;
        function balanceOf(address account) external view returns (uint256);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn local_gas_estimate_matches_node() {
    let rpc =
        std::env::var("BASE_RPC_HTTP").unwrap_or_else(|_| "https://mainnet.base.org".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc.parse().unwrap());
    let block = provider.get_block_number().await.unwrap();
    let block_id = revm::database::BlockId::number(block);

    // Case 1: simple state-mutating call — WETH.deposit{value: 1 wei}.
    // Node estimate includes the 21k intrinsic; the local sim executes the
    // full tx too, so both must land within the node's inherent margin.
    let weth = Address::from_str(WETH).unwrap();
    let depositor = Address::from_str(BASE_BRIDGE).unwrap(); // funded EOA
    let calldata = IWETH::depositCall {}.abi_encode();

    let node_gas = provider
        .estimate_gas(
            TransactionRequest::default()
                .from(depositor)
                .to(weth)
                .value(U256::from(1u64))
                .input(calldata.clone().into()),
        )
        .block(alloy::eips::BlockId::number(block))
        .await
        .expect("node eth_estimateGas succeeds");

    let env = fetch_sim_env(&provider, block_id)
        .await
        .expect("sim block context resolves");
    let outcome = simulate_call(
        provider.clone(),
        weth,
        depositor,
        Bytes::from(calldata),
        block_id,
        Some(env),
    )
    .expect("local sim executes");
    let local_gas = match outcome {
        SimOutcome::Success { gas_used, .. } => gas_used,
        SimOutcome::Reverted(reason) => panic!("local sim reverted: {reason}"),
    };

    // Both numbers price the same execution; revm uses exact gas (no
    // estimator padding), the node may add margin. Allow +/-25%.
    let lo = node_gas * 3 / 4;
    let hi = node_gas * 5 / 4;
    assert!(
        local_gas >= lo && local_gas <= hi,
        "local {local_gas} vs node {node_gas} diverge >25%"
    );

    // Case 2: revert parity — approve() on the Slipstream factory (contract
    // has no such selector and no fallback) reverts on both paths; the local
    // sim must surface it as Reverted with the same verdict as the node.
    alloy::sol! {
        #[sol(rpc)]
        interface IBogus {
            function approve(address spender, uint256 amount) external returns (bool);
        }
    }
    let factory = Address::from_str("0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A").unwrap();
    let bogus = IBogus::approveCall {
        spender: Address::ZERO,
        amount: U256::from(1u64),
    }
    .abi_encode();
    let node_revert = provider
        .call(
            TransactionRequest::default()
                .from(depositor)
                .to(factory)
                .input(bogus.clone().into()),
        )
        .block(alloy::eips::BlockId::number(block))
        .await;
    assert!(node_revert.is_err(), "node must revert on unknown selector");
    let env = fetch_sim_env(&provider, block_id)
        .await
        .expect("sim block context resolves");
    let outcome = simulate_call(
        provider,
        factory,
        depositor,
        Bytes::from(bogus),
        block_id,
        Some(env),
    )
    .expect("local sim executes");
    match outcome {
        SimOutcome::Success { .. } => panic!("expected revert on unknown selector"),
        SimOutcome::Reverted(_) => {}
    }
}
