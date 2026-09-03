//! Live-chain integration test validating the local CL swap simulation
//! against the on-chain Slipstream Quoter. `#[ignore]` by default — it hits
//! a public RPC — so run it explicitly with:
//!
//! ```sh
//! cargo test --test chain_cl -- --ignored
//! ```

use alloy::primitives::{Address, U256};
use alloy::providers::ProviderBuilder;
use morpho_arbitrage_bot::cl_math::cl_quote_exact_in;
use morpho_arbitrage_bot::dex::{fetch_cl_pair_tokens, fetch_quotes, QuoteRequest};
use morpho_arbitrage_bot::state::{bootstrap_cl, PoolState};
use std::str::FromStr;

const SLIPSTREAM_FACTORY: &str = "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A";
const SLIPSTREAM_QUOTER: &str = "0x254cf9e1e6e233aa1ac962cb9b05b2cfeaae15b0";
const WETH: &str = "0x4200000000000000000000000000000000000006";
const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

alloy::sol! {
    #[sol(rpc)]
    interface ISlipstreamFactory {
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address pool);
    }
}

#[tokio::test]
#[ignore = "hits a live Base RPC; run explicitly with --ignored"]
async fn local_cl_quote_matches_slipstream_quoter() {
    let rpc = std::env::var("BASE_RPC_HTTP")
        .unwrap_or_else(|_| "https://mainnet.base.org".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc.parse().unwrap());

    // ts=100 WETH/USDC is an active Slipstream pool on Base.
    let weth = Address::from_str(WETH).unwrap();
    let virtual_ = Address::from_str(USDC).unwrap();
    let factory = ISlipstreamFactory::new(
        Address::from_str(SLIPSTREAM_FACTORY).unwrap(),
        &provider,
    );
    let pool = factory
        .getPool(weth, virtual_, alloy::primitives::aliases::I24::try_from(100).unwrap())
        .call()
        .await
        .expect("factory call failed");
    assert_ne!(pool, Address::ZERO, "pool does not exist");

    let state = bootstrap_cl(&provider, pool).await.expect("bootstrap failed");
    let PoolState::Cl {
        liquidity, ticks, ..
    } = &state
    else {
        panic!("expected CL state");
    };
    assert!(*liquidity > 0, "pool has no liquidity");
    assert!(!ticks.is_empty(), "bootstrap found no initialized ticks");

    let tokens = fetch_cl_pair_tokens(&provider, pool).await.expect("token fetch");
    // Sell WETH -> USDC.
    let zero_for_one = tokens.token0 == weth;
    let amount_in = U256::from(10_000_000_000_000_000u64); // 0.01 WETH

    let local = cl_quote_exact_in(&state, zero_for_one, amount_in).expect("local quote failed");

    let on_chain = fetch_quotes(
        &provider,
        &[QuoteRequest {
            token_in: weth,
            token_out: virtual_,
            fee_tier: 100,
            amount_in,
            quoter: Address::from_str(SLIPSTREAM_QUOTER).unwrap(),
            slipstream: true,
        }],
        alloy::eips::BlockId::latest(),
    )
    .await
    .expect("quoter batch failed")[0]
    .expect("on-chain quoter reverted");

    assert_eq!(
        local, on_chain,
        "local CL quote diverges from on-chain Slipstream Quoter"
    );
}
