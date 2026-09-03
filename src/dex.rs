use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::Result;
use tracing::debug;

/// Whether the RPC endpoint exposes Flashblock preconfirmed state via the
/// `pending` block tag. This is a heuristic: a node that streams Flashblocks
/// reports a `pending` block whose number is *ahead* of `latest` (it is the
/// in-progress sealed block being built from 200ms sub-blocks). A plain node
/// without Flashblocks may still number its pending candidate `latest + 1`,
/// so this is best treated as a soft signal rather than a hard guarantee.
///
/// For a stronger, Flashblock-specific check on a WebSocket endpoint, prefer
/// [`probe_flashblocks_ws`], which tries the non-standard `newFlashblocks`
/// subscription. This HTTP heuristic is the fallback when only an HTTP RPC
/// is available (the broadcaster path).
///
/// Returns `true` only when `pending` is strictly ahead of `latest`, so a
/// node that returns `pending == latest` (the common non-Flashblock case) is
/// correctly classified as not streaming Flashblocks.
pub async fn pending_is_fresher<P: Provider>(provider: &P) -> bool {
    let pending = provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Pending)
        .await;
    let latest = provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
        .await;
    match (pending, latest) {
        (Ok(Some(p)), Ok(Some(l))) => p.header.number > l.header.number,
        _ => false,
    }
}

/// Pick the block id to pin chain reads to. When Flashblock preconfirmed
/// state is both enabled and available, reads the ~200ms-fresh `pending`
/// tag; otherwise falls back to a sealed `latest` block number. Sealed
/// blocks never reorg, so the two-phase scan stays consistent.
///
/// `flashblocks_available` is a startup-probed, cached capability decision
/// (see [`pending_is_fresher`]) so the per-scan path makes no extra RPC
/// calls. When `None`, the probe is run inline (kept for the `once` path
/// which has no shared cache).
pub async fn read_block_id<P: Provider>(
    provider: &P,
    use_pending: bool,
    flashblocks_available: Option<bool>,
) -> Result<alloy::eips::BlockId> {
    let want_pending = use_pending
        && match flashblocks_available {
            Some(ok) => ok,
            None => pending_is_fresher(provider).await,
        };
    if want_pending {
        Ok(alloy::eips::BlockId::pending())
    } else {
        let block_number = provider.get_block_number().await?;
        Ok(alloy::eips::BlockId::number(block_number))
    }
}

/// The sealed `latest` block number observed alongside a pending read. When
/// scanning preconfirmed state, the scan's watermark is the in-progress
/// sealed block (the `pending` tag maps to it), so callers track that number
/// for event-loop bookkeeping rather than the mutable `pending` tag.
pub async fn latest_block_number<P: Provider>(provider: &P) -> Result<u64> {
    Ok(provider.get_block_number().await?)
}

/// Probe a WebSocket endpoint for the Flashblock-specific subscription
/// `newFlashblocks`. Unlike the `pending`-vs-`latest` block-number heuristic,
/// this subscription method is only implemented by Flashblock-aware nodes
/// (it is absent from stock OP-Stack clients), so a successful subscribe is
/// a strong, Flashblock-specific signal. The subscription is immediately
/// dropped (we only care that it was accepted); returns `true` on success.
///
/// Falls back to `false` on any error (method not found, non-pubsub HTTP
/// provider, etc.) so the caller degrades to sealed-block behavior. Kept
/// generic over `Provider` so the WS provider built for event-driven mode
/// can be reused for the probe.
pub async fn probe_flashblocks_ws<P: Provider>(provider: &P) -> bool {
    // eth_subscribe with kind "newFlashblocks" and no params. A non-Flashblock
    // node rejects the method, which surfaces as an error from the subscribe
    // call. We only care that the subscription was accepted; dropping the
    // returned `Subscription` releases it (the pubsub frontend tracks local
    // subscriptions and tears them down on drop).
    provider
        .subscribe::<(String,), alloy::primitives::Bytes>(("newFlashblocks".to_string(),))
        .await
        .is_ok()
}

sol! {
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }

    #[sol(rpc)]
    interface IUniswapV3Pool {
        function token0() external view returns (address);
        function token1() external view returns (address);
    }

    // CL pool state reads (V3 + Slipstream share this layout; needed to
    // bootstrap the local PoolState in state.rs/cl_math.rs).
    #[sol(rpc)]
    interface IClPoolState {
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function tickBitmap(int16 wordPosition) external view returns (uint256);
        function ticks(int24 tick) external view returns (uint128 liquidityGross, int128 liquidityNet, uint256 feeGrowthOutside0X128, uint256 feeGrowthOutside1X128, int56 tickCumulativeOutside, uint160 secondsPerLiquidityOutsideX128, uint32 tickCumulativeOutside1, uint160 secondsPerLiquidityOutsideX128_2, uint32 tickCumulativeOutside2, bool initialized);
    }

    struct QuoteExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }

    // Uniswap QuoterV2: not view (it simulates the swap), so always called
    // via eth_call; it returns real values rather than packed revert data.
    #[sol(rpc)]
    interface IQuoterV2 {
        function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
            external
            returns (
                uint256 amountOut,
                uint160[] memory sqrtPriceX96AfterList,
                uint32[] memory initializedTicksCrossedList,
                uint256 gasEstimate
            );
    }

    // Aerodrome Slipstream CL pool state, used for on-chain validation.
    #[sol(rpc)]
    interface ICLPool {
        function token0() external view returns (address);
        function token1() external view returns (address);
    }

    struct QuoteExactInputSingleClParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        int24 tickSpacing;
        uint160 sqrtPriceLimitX96;
    }

    // Aerodrome Slipstream Quoter (0x254cF9E1...15b0 on Base): same shape as
    // Uniswap QuoterV2 but discriminates pools by tickSpacing, not fee. Not
    // `view` — must be eth_call.
    #[sol(rpc)]
    interface IQuoterSlipstream {
        function quoteExactInputSingle(QuoteExactInputSingleClParams memory params)
            external
            returns (
                uint256 amountOut,
                uint160 sqrtPriceX96After,
                uint32 initializedTicksCrossed,
                uint256 gasEstimate
            );
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

    // Aerodrome Slipstream CL pool factory (0x5e7BB104...5809A on Base).
    #[sol(rpc)]
    interface ISlipstreamFactory {
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address pool);
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
        VenueKind::Slipstream => {
            ISlipstreamFactory::new(factory, provider)
                .getPool(
                    token_a,
                    token_b,
                    alloy::primitives::aliases::I24::try_from(fee_tier).map_err(|_| {
                        eyre::eyre!("slipstream tickSpacing {fee_tier} out of i24 range")
                    })?,
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

    let (r0, r1) = (U256::from(reserves.reserve0), U256::from(reserves.reserve1));
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

/// One QuoterV2 `quoteExactInputSingle` request. The fee tier must be the
/// venue's actual pool fee — quoting with a different tier prices a
/// different pool. For Slipstream venues the field carries tickSpacing and
/// the call goes through `IQuoterSlipstream`.
#[derive(Debug, Clone, Copy)]
pub struct QuoteRequest {
    pub token_in: Address,
    pub token_out: Address,
    pub fee_tier: u32,
    pub amount_in: U256,
    /// QuoterV2 contract to call; per-request so a single batch can price
    /// venues whose V3 quotes live on different quoter deployments (e.g.
    /// Uniswap vs PancakeSwap), which have distinct factories and therefore
    /// distinct quoter contracts.
    pub quoter: Address,
    /// When true, encode with `IQuoterSlipstream` (int24 tickSpacing)
    /// instead of `IQuoterV2` (uint24 fee). Set per venue kind.
    pub slipstream: bool,
}

/// Per-scan bundle of everything the bot needs from the chain, fetched in
/// ONE JSON-RPC batch: getReserves per V2/Aero venue, one QuoterV2 call per
/// requested V3 quote, and the current gas price.
pub struct ScanSnapshot {
    /// Raw (reserve0, reserve1) per V2/Aero venue, aligned with the
    /// `v2_venues` slice passed to `fetch_scan_snapshot`. None when that
    /// venue's eth_call reverted (caller skips it instead of aborting).
    pub v2_raw: Vec<Option<(U256, U256)>>,
    /// QuoterV2 amountOut per entry of the `quotes` slice passed to
    /// `fetch_scan_snapshot`; None when that quote reverted (e.g. the trade
    /// exceeds the pool's liquidity).
    pub v3_quotes: Vec<Option<U256>>,
    pub gas_price: U256,
    /// The block the snapshot was pinned to, when it was read from a
    /// numbered block (None for a pending-tag pin). Local pool-state
    /// refreshes are pinned to the same block so every leg prices off the
    /// exact same chain state.
    pub pinned_block: Option<u64>,
}

/// Encode one quote request as an eth_call transaction against its quoter.
fn quote_tx(req: &QuoteRequest) -> alloy::rpc::types::eth::TransactionRequest {
    use alloy::rpc::types::eth::TransactionRequest;
    let input: Bytes = if req.slipstream {
        IQuoterSlipstream::quoteExactInputSingleCall {
            params: QuoteExactInputSingleClParams {
                tokenIn: req.token_in,
                tokenOut: req.token_out,
                amountIn: req.amount_in,
                // Config validation already bounds slipstream fee_tier to
                // {1, 50, 100, 200, 2000}, well inside i24.
                tickSpacing: alloy::primitives::aliases::I24::try_from(req.fee_tier)
                    .expect("slipstream tickSpacing validated at config"),
                sqrtPriceLimitX96: Default::default(),
            },
        }
        .abi_encode()
        .into()
    } else {
        IQuoterV2::quoteExactInputSingleCall {
            params: QuoteExactInputSingleParams {
                tokenIn: req.token_in,
                tokenOut: req.token_out,
                amountIn: req.amount_in,
                fee: alloy::primitives::Uint::<24, 1>::from(req.fee_tier),
                sqrtPriceLimitX96: Default::default(),
            },
        }
        .abi_encode()
        .into()
    };
    TransactionRequest::default()
        .to(req.quoter)
        .input(input.into())
}

fn decode_quote(raw: &Bytes) -> Option<U256> {
    // QuoterV2 returns (amountOut, sqrtPriceX96AfterList,
    // initializedTicksCrossedList, gasEstimate); alloy's abi_decode_returns
    // is strict about trailing words, and older call sites may only model
    // amountOut — decode the first word directly so a well-formed quote is
    // never discarded just because the tail fields are present.
    if raw.len() < 32 {
        return None;
    }
    Some(U256::from_be_slice(&raw[..32]))
}

/// Fetch a full scan snapshot in a single JSON-RPC batch. Every eth_call
/// carries an explicit block id (Chainstack rejects batch calls without
/// one); the caller pins the block so this snapshot and any follow-up
/// quote batch are consistent with each other.
pub async fn fetch_scan_snapshot<P: Provider>(
    provider: &P,
    v2_venues: &[Address], // pair addresses
    quotes: &[QuoteRequest],
    block: alloy::eips::BlockId,
) -> Result<ScanSnapshot> {
    use alloy::rpc::types::eth::TransactionRequest;

    let mut batch = alloy::rpc::client::BatchRequest::new(provider.client());
    let mut waiters = Vec::with_capacity(v2_venues.len() + quotes.len() + 1);

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
    let mut quote_waiters = Vec::with_capacity(quotes.len());
    for req in quotes {
        quote_waiters.push(
            batch
                .add_call::<_, Bytes>("eth_call", &(quote_tx(req), block))
                .map_err(eyre::Error::from)?,
        );
    }
    let gas_price_waiter = batch
        .add_call::<_, U256>("eth_gasPrice", &())
        .map_err(eyre::Error::from)?;

    batch.send().await.map_err(eyre::Error::from)?;

    // Per-venue error handling: a single reverted call (dead/misconfigured
    // pool) yields None for that venue instead of failing the whole scan.
    let mut v2_raw = Vec::with_capacity(v2_venues.len());
    for waiter in waiters {
        let entry = match waiter.await {
            Ok(raw) => IUniswapV2Pair::getReservesCall::abi_decode_returns(&raw)
                .ok()
                .map(|r| (U256::from(r.reserve0), U256::from(r.reserve1))),
            Err(_) => None,
        };
        v2_raw.push(entry);
    }
    let mut v3_quotes = Vec::with_capacity(quotes.len());
    for (waiter, req) in quote_waiters.into_iter().zip(quotes.iter()) {
        v3_quotes.push(match waiter.await {
            Ok(raw) => match decode_quote(&raw) {
                Some(q) => Some(q),
                None => {
                    debug!(
                        token_in = %req.token_in,
                        token_out = %req.token_out,
                        fee_tier = req.fee_tier,
                        amount_in = %req.amount_in,
                        "V3 quote returned undecodable result"
                    );
                    None
                }
            },
            Err(e) => {
                debug!(
                    token_in = %req.token_in,
                    token_out = %req.token_out,
                    fee_tier = req.fee_tier,
                    amount_in = %req.amount_in,
                    error = %e,
                    "V3 quote reverted"
                );
                None
            }
        });
    }
    let gas_price = gas_price_waiter.await.map_err(eyre::Error::from)?;

    Ok(ScanSnapshot {
        v2_raw,
        v3_quotes,
        gas_price,
        pinned_block: block.as_u64(),
    })
}

/// Run a standalone batch of QuoterV2 quotes (used for leg 2, whose inputs
/// are only known after leg 1 has been priced). None per reverted quote.
/// Pinned to the same block as the phase-1 snapshot.
pub async fn fetch_quotes<P: Provider>(
    provider: &P,
    requests: &[QuoteRequest],
    block: alloy::eips::BlockId,
) -> Result<Vec<Option<U256>>> {
    let mut batch = alloy::rpc::client::BatchRequest::new(provider.client());
    let mut waiters = Vec::with_capacity(requests.len());
    for req in requests {
        waiters.push(
            batch
                .add_call::<_, Bytes>("eth_call", &(quote_tx(req), block))
                .map_err(eyre::Error::from)?,
        );
    }
    batch.send().await.map_err(eyre::Error::from)?;

    let mut out = Vec::with_capacity(requests.len());
    for waiter in waiters {
        out.push(match waiter.await {
            Ok(raw) => decode_quote(&raw),
            Err(_) => None,
        });
    }
    Ok(out)
}

/// Validate at startup that a V3 pool actually contains the two configured
/// tokens (pricing thereafter goes through QuoterV2, which takes the token
/// direction explicitly, so no orientation state is kept).
pub async fn fetch_v3_pair_tokens<P: Provider>(provider: &P, pool: Address) -> Result<PairTokens> {
    let pool_contract = IUniswapV3Pool::new(pool, provider);
    let token0 = pool_contract.token0().call().await?;
    let token1 = pool_contract.token1().call().await?;
    Ok(PairTokens { token0, token1 })
}

/// Same as `fetch_v3_pair_tokens` but for a Slipstream CL pool (token0/token1
/// are the same standard getters; only the factory/quoter ABIs differ).
pub async fn fetch_cl_pair_tokens<P: Provider>(provider: &P, pool: Address) -> Result<PairTokens> {
    let pool_contract = ICLPool::new(pool, provider);
    let token0 = pool_contract.token0().call().await?;
    let token1 = pool_contract.token1().call().await?;
    Ok(PairTokens { token0, token1 })
}
