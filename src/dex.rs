use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::Result;

sol! {
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }

    #[sol(rpc)]
    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }

    #[sol(rpc)]
    interface IUniswapV3Quoter {
        function quoteExactInputSingle(
            address tokenIn,
            address tokenOut,
            uint24 fee,
            uint256 amountIn,
            uint160 sqrtPriceLimitX96
        ) external returns (uint256 amountOut);
    }
}

/// Reserves of a V2-style pool, normalized so `reserve_in` always corresponds
/// to the token being sold and `reserve_out` to the token being bought.
#[derive(Debug, Clone, Copy)]
pub struct PoolReserves {
    pub reserve_in: U256,
    pub reserve_out: U256,
}

/// Constant-product swap output with a configurable fee in basis points
/// (e.g. 30 = 0.3% for Uniswap V2, 5 = 0.05% for an Aerodrome pool).
pub fn get_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u64,
) -> Option<U256> {
    if fee_bps >= 10_000 {
        return None;
    }
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return None;
    }
    let scale = U256::from(10_000u64);
    let amount_in_with_fee = amount_in.checked_mul(U256::from(10_000u64 - fee_bps))?;
    let numerator = amount_in_with_fee.checked_mul(reserve_out)?;
    let denominator = reserve_in
        .checked_mul(scale)?
        .checked_add(amount_in_with_fee)?;
    Some(numerator / denominator)
}

/// Fetch live reserves for `pair`, oriented relative to `token_in`. Fails if
/// `token_in` is not one of the pool's two tokens, instead of silently
/// assuming the orientation.
pub async fn fetch_reserves<P: Provider>(
    provider: &P,
    pair: Address,
    token_in: Address,
) -> Result<PoolReserves> {
    let pool = IUniswapV2Pair::new(pair, provider);
    let reserves = pool.getReserves().call().await?;
    let token0 = pool.token0().call().await?;
    let token1 = pool.token1().call().await?;

    let (r0, r1) = (
        U256::from(reserves.reserve0),
        U256::from(reserves.reserve1),
    );
    let (reserve_in, reserve_out) = if token_in == token0 {
        (r0, r1)
    } else if token_in == token1 {
        (r1, r0)
    } else {
        return Err(eyre::eyre!(
            "pair {pair} does not contain token {token_in} (token0={token0}, token1={token1})"
        ));
    };
    Ok(PoolReserves {
        reserve_in,
        reserve_out,
    })
}

/// Fetch reserves for multiple venues in a single JSON-RPC batch for
/// block-aligned snapshots. Each venue contributes 3 calls
/// (getReserves + token0 + token1) to one batch, so all pools are read
/// against the same block at the node.
pub async fn fetch_reserves_batched<P: Provider>(
    provider: &P,
    venues: &[(Address, Address)], // (pair, token_in) pairs
) -> Result<Vec<PoolReserves>> {
    use alloy::eips::BlockNumberOrTag;
    use alloy::rpc::types::eth::TransactionRequest;

    // Pin every call to "latest"; Chainstack requires the block argument, and
    // an explicit tag keeps all reads in the batch against the same block.
    let block = BlockNumberOrTag::Latest;
    let mut batch = alloy::rpc::client::BatchRequest::new(provider.client());
    let mut waiters = Vec::with_capacity(venues.len() * 3);
    for (pair, _) in venues {
        for data in [
            IUniswapV2Pair::getReservesCall {}.abi_encode(),
            IUniswapV2Pair::token0Call {}.abi_encode(),
            IUniswapV2Pair::token1Call {}.abi_encode(),
        ] {
            let tx = TransactionRequest::default().to(*pair).input(data.into());
            waiters.push(
                batch
                    .add_call("eth_call", &(tx, block))
                    .map_err(eyre::Error::from)?,
            );
        }
    }
    batch.send().await.map_err(eyre::Error::from)?;

    let mut waiters = waiters.into_iter();
    let mut out = Vec::with_capacity(venues.len());
    for (i, (_, token_in)) in venues.iter().enumerate() {
        let r_raw: Bytes = waiters.next().expect("3 waiters per venue").await.map_err(eyre::Error::from)?;
        let t0_raw: Bytes = waiters.next().expect("3 waiters per venue").await.map_err(eyre::Error::from)?;
        let t1_raw: Bytes = waiters.next().expect("3 waiters per venue").await.map_err(eyre::Error::from)?;
        let reserves = IUniswapV2Pair::getReservesCall::abi_decode_returns(&r_raw)?;
        let token0 = IUniswapV2Pair::token0Call::abi_decode_returns(&t0_raw)?;
        let token1 = IUniswapV2Pair::token1Call::abi_decode_returns(&t1_raw)?;

        let (r0, r1) = (
            U256::from(reserves.reserve0),
            U256::from(reserves.reserve1),
        );
        let (reserve_in, reserve_out) = if *token_in == token0 {
            (r0, r1)
        } else if *token_in == token1 {
            (r1, r0)
        } else {
            return Err(eyre::eyre!(
                "pair {} does not contain token {token_in} (token0={token0}, token1={token1})",
                venues[i].0
            ));
        };
        out.push(PoolReserves {
            reserve_in,
            reserve_out,
        });
    }
    Ok(out)
}

/// Fetch V3 pool state (sqrtPriceX96, liquidity) for price calculation.
pub async fn fetch_v3_pool_state<P: Provider>(
    provider: &P,
    pool: Address,
    token_in: Address,
) -> Result<(U256, U256)> {
    let pool_contract = IUniswapV3Pool::new(pool, provider);
    let slot0 = pool_contract.slot0().call().await?;
    let liquidity = pool_contract.liquidity().call().await?;
    let token0 = pool_contract.token0().call().await?;
    let token1 = pool_contract.token1().call().await?;

    if token_in != token0 && token_in != token1 {
        return Err(eyre::eyre!(
            "V3 pool {pool} does not contain token {token_in} (token0={token0}, token1={token1})"
        ));
    }

    Ok((U256::from(slot0.sqrtPriceX96), U256::from(liquidity)))
}

/// Quote V3 output using the quoter contract (more accurate than local math).
pub async fn quote_v3_output<P: Provider>(
    provider: &P,
    quoter: Address,
    token_in: Address,
    token_out: Address,
    fee_tier: u32,
    amount_in: U256,
) -> Result<U256> {
    let quoter_contract = IUniswapV3Quoter::new(quoter, provider);
    let amount_out = quoter_contract
        .quoteExactInputSingle(
            token_in,
            token_out,
            alloy::primitives::Uint::<24, 1>::from(fee_tier),
            amount_in,
            alloy::primitives::Uint::<160, 3>::ZERO,
        )
        .call()
        .await?;
    Ok(amount_out)
}
