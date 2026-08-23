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
    interface IUniswapV2Factory {
        function getPair(address tokenA, address tokenB) external view returns (address pair);
    }

    #[sol(rpc)]
    interface IAerodromeFactory {
        function getPool(address tokenA, address tokenB, bool stable) external view returns (address pool);
    }

    #[sol(rpc)]
    interface IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
    }

    // Aerodrome router can resolve its default factory.
    #[sol(rpc)]
    interface IAerodromeRouter {
        function defaultFactory() external view returns (address);
    }
}

/// Reserves of a V2-style pool, normalized so `reserve_in` always corresponds
/// to the token being sold and `reserve_out` to the token being bought.
#[derive(Debug, Clone, Copy)]
pub struct PoolReserves {
    pub reserve_in: U256,
    pub reserve_out: U256,
}

/// One-off, immutable pool metadata resolved at startup so per-scan batches
/// only need `getReserves` (token0/token1 never change for a pair).
#[derive(Debug, Clone, Copy)]
pub struct PairTokens {
    pub token0: Address,
    pub token1: Address,
}

/// Resolve (token0, token1) for a V2-style pair. Called once at startup.
pub async fn fetch_pair_tokens<P: Provider>(provider: &P, pair: Address) -> Result<PairTokens> {
    let pool = IUniswapV2Pair::new(pair, provider);
    let token0 = pool.token0().call().await?;
    let token1 = pool.token1().call().await?;
    Ok(PairTokens { token0, token1 })
}

/// Inputs for resolving a pool address from a venue's factory.
pub struct PoolQuery {
    pub kind: crate::config::VenueKind,
    pub factory: Address,
    pub router: Address,
    pub token_a: Address,
    pub token_b: Address,
    pub stable: bool,
    pub fee_tier: u32,
}

/// Resolve the pool address for a token pair from a venue's factory.
/// `factory` of zero for Aerodrome means the router's default factory
/// (resolved on-chain).
pub async fn resolve_pool<P: Provider>(provider: &P, q: &PoolQuery) -> Result<Address> {
    let PoolQuery {
        kind,
        factory,
        router,
        token_a,
        token_b,
        stable,
        fee_tier,
    } = *q;
    use crate::config::VenueKind;
    let pool = match kind {
        VenueKind::UniswapV2 => {
            IUniswapV2Factory::new(factory, provider)
                .getPair(token_a, token_b)
                .call()
                .await?
        }
        VenueKind::Aerodrome => {
            let factory = if factory == Address::ZERO {
                IAerodromeRouter::new(router, provider)
                    .defaultFactory()
                    .call()
                    .await?
            } else {
                factory
            };
            IAerodromeFactory::new(factory, provider)
                .getPool(token_a, token_b, stable)
                .call()
                .await?
        }
        VenueKind::UniswapV3 => {
            IUniswapV3Factory::new(factory, provider)
                .getPool(
                    token_a,
                    token_b,
                    alloy::primitives::Uint::<24, 1>::from(fee_tier),
                )
                .call()
                .await?
        }
        VenueKind::UniswapV4 => {
            return Err(eyre::eyre!("V4 pool auto-resolution is not supported"));
        }
    };
    if pool == Address::ZERO {
        return Err(eyre::eyre!(
            "factory {factory} has no pool for {token_a}/{token_b}"
        ));
    }
    Ok(pool)
}

/// Orient raw reserves relative to `token_in` using cached pair tokens.
pub fn orient_reserves(
    reserve0: U256,
    reserve1: U256,
    tokens: &PairTokens,
    pair: Address,
    token_in: Address,
) -> Result<PoolReserves> {
    let (reserve_in, reserve_out) = if token_in == tokens.token0 {
        (reserve0, reserve1)
    } else if token_in == tokens.token1 {
        (reserve1, reserve0)
    } else {
        return Err(eyre::eyre!(
            "pair {pair} does not contain token {token_in} (token0={}, token1={})",
            tokens.token0,
            tokens.token1
        ));
    };
    Ok(PoolReserves {
        reserve_in,
        reserve_out,
    })
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

/// Per-scan bundle of everything the bot needs from the chain, fetched in
/// ONE JSON-RPC batch: getReserves per V2/Aero venue, slot0+liquidity per
/// V3 venue, and the current gas price.
pub struct ScanSnapshot {
    /// Raw (reserve0, reserve1) per V2/Aero venue, aligned with the
    /// `v2_venues` slice passed to `fetch_scan_snapshot`. None when that
    /// venue's eth_call reverted (caller skips it instead of aborting).
    pub v2_raw: Vec<Option<(U256, U256)>>,
    /// (sqrtPriceX96, liquidity) per V3 venue; None on per-venue failure.
    pub v3_raw: Vec<Option<(U256, U256)>>,
    pub gas_price: U256,
}

/// Fetch a full scan snapshot in a single JSON-RPC batch. Requires
/// `eth_call` with an explicit block tag ("latest") — Chainstack rejects
/// batch calls without it — and keeps all reads block-aligned.
pub async fn fetch_scan_snapshot<P: Provider>(
    provider: &P,
    v2_venues: &[Address], // pair addresses
    v3_venues: &[Address], // pool addresses
) -> Result<ScanSnapshot> {
    use alloy::eips::BlockNumberOrTag;
    use alloy::rpc::types::eth::TransactionRequest;

    let block = BlockNumberOrTag::Latest;
    let mut batch = alloy::rpc::client::BatchRequest::new(provider.client());
    let mut waiters = Vec::with_capacity(v2_venues.len() + v3_venues.len() * 2 + 1);

    for pair in v2_venues {
        let tx = TransactionRequest::default()
            .to(*pair)
            .input(IUniswapV2Pair::getReservesCall {}.abi_encode().into());
        waiters.push(
            batch
                .add_call::<_, Bytes>("eth_call", &(tx, block))
                .map_err(eyre::Error::from)?,
        );
    }
    for pool in v3_venues {
        for data in [
            IUniswapV3Pool::slot0Call {}.abi_encode(),
            IUniswapV3Pool::liquidityCall {}.abi_encode(),
        ] {
            let tx = TransactionRequest::default().to(*pool).input(data.into());
            waiters.push(
                batch
                    .add_call::<_, Bytes>("eth_call", &(tx, block))
                    .map_err(eyre::Error::from)?,
            );
        }
    }
    let gas_price_waiter = batch
        .add_call::<_, U256>("eth_gasPrice", &())
        .map_err(eyre::Error::from)?;

    batch.send().await.map_err(eyre::Error::from)?;

    // Per-venue error handling: a single reverted call (dead/misconfigured
    // pool) yields None for that venue instead of failing the whole scan.
    let mut waiters = waiters.into_iter();
    let mut v2_raw = Vec::with_capacity(v2_venues.len());
    for _ in v2_venues {
        let entry: Option<(U256, U256)> =
            match waiters.next().expect("one waiter per V2 venue").await {
                Ok(raw) => IUniswapV2Pair::getReservesCall::abi_decode_returns(&raw)
                    .ok()
                    .map(|r| (U256::from(r.reserve0), U256::from(r.reserve1))),
                Err(_) => None,
            };
        v2_raw.push(entry);
    }
    let mut v3_raw = Vec::with_capacity(v3_venues.len());
    for _ in v3_venues {
        let entry: Option<(U256, U256)> = match (
            waiters.next().expect("two waiters per V3 venue").await,
            waiters.next().expect("two waiters per V3 venue").await,
        ) {
            (Ok(s_raw), Ok(l_raw)) => {
                match (
                    IUniswapV3Pool::slot0Call::abi_decode_returns(&s_raw),
                    IUniswapV3Pool::liquidityCall::abi_decode_returns(&l_raw),
                ) {
                    (Ok(slot0), Ok(liquidity)) => {
                        Some((U256::from(slot0.sqrtPriceX96), U256::from(liquidity)))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        v3_raw.push(entry);
    }
    let gas_price = gas_price_waiter.await.map_err(eyre::Error::from)?;

    Ok(ScanSnapshot {
        v2_raw,
        v3_raw,
        gas_price,
    })
}

/// Fetch V3 pool state (sqrtPriceX96, liquidity) for price calculation.
/// Serial fallback used when the batched snapshot fails.
pub async fn fetch_v3_pool_state<P: Provider>(
    provider: &P,
    pool: Address,
) -> Result<(U256, U256)> {
    let pool_contract = IUniswapV3Pool::new(pool, provider);
    let slot0 = pool_contract.slot0().call().await?;
    let liquidity = pool_contract.liquidity().call().await?;
    Ok((U256::from(slot0.sqrtPriceX96), U256::from(liquidity)))
}

/// Validate at startup that a V3 pool actually contains the two configured
/// tokens (cached pair tokens are used for orientation thereafter).
pub async fn fetch_v3_pair_tokens<P: Provider>(provider: &P, pool: Address) -> Result<PairTokens> {
    let pool_contract = IUniswapV3Pool::new(pool, provider);
    let token0 = pool_contract.token0().call().await?;
    let token1 = pool_contract.token1().call().await?;
    Ok(PairTokens { token0, token1 })
}


