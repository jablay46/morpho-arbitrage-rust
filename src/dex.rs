use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use eyre::Result;

sol! {
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }
}

/// Reserves of a V2-style pool, normalized so `reserve_in` always corresponds
/// to the token being sold and `reserve_out` to the token being bought.
#[derive(Debug, Clone, Copy)]
pub struct PoolReserves {
    pub reserve_in: U256,
    pub reserve_out: U256,
}

/// Constant-product swap output with the standard 0.3% fee (997/1000).
pub fn get_amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256) -> Option<U256> {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return None;
    }
    let amount_in_with_fee = amount_in.checked_mul(U256::from(997u64))?;
    let numerator = amount_in_with_fee.checked_mul(reserve_out)?;
    let denominator = reserve_in
        .checked_mul(U256::from(1000u64))?
        .checked_add(amount_in_with_fee)?;
    Some(numerator / denominator)
}

/// Fetch live reserves for `pair`, oriented relative to `token_in`.
pub async fn fetch_reserves<P: Provider>(
    provider: &P,
    pair: Address,
    token_in: Address,
) -> Result<PoolReserves> {
    let pool = IUniswapV2Pair::new(pair, provider);
    let reserves = pool.getReserves().call().await?;
    let token0 = pool.token0().call().await?;

    let (r0, r1) = (
        U256::from(reserves.reserve0),
        U256::from(reserves.reserve1),
    );
    let (reserve_in, reserve_out) = if token0 == token_in { (r0, r1) } else { (r1, r0) };
    Ok(PoolReserves {
        reserve_in,
        reserve_out,
    })
}
