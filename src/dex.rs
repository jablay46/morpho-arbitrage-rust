use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::eth::TransactionRequest;
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

    // Multicall3 (canonical deployment, same address on Base and most EVM
    // chains): runs many eth_calls inside ONE RPC request. Providers bill
    // JSON-RPC batches per sub-call, but an aggregate3 eth_call is a single
    // request no matter how many sub-calls it carries.
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] memory calls) external payable returns (Result[] memory returnData);
    }
}

/// Canonical Multicall3 deployment address (Base mainnet and most chains).
pub const MULTICALL3_ADDRESS: Address =
    alloy::primitives::address!("cA11bde05977b3631167028862bE2a173976CA11");

/// Sub-calls per aggregate3 request: bounds the eth_call gas and response
/// size so a large call set (e.g. hundreds of `ticks()` reads) neither hits
/// the provider's eth_call gas cap nor its response-size limit.
const MULTICALL_CHUNK: usize = 256;

/// Execute (target, calldata) reads pinned to `block` as a handful of RPC
/// requests via Multicall3 `aggregate3`: each sub-call is allowed to fail
/// independently, and per-call outcomes are returned in order (`Err`
/// carries the revert data). RPC-level failures are propagated to the
/// caller; a plain JSON-RPC batch is used only when aggregate3 returns
/// undecodable output, i.e. the chain has no Multicall3 deployment.
pub async fn run_eth_calls<P: Provider>(
    provider: &P,
    calls: &[(Address, Bytes)],
    block: alloy::eips::BlockId,
) -> Result<Vec<std::result::Result<Bytes, Bytes>>> {
    let mut out: Vec<std::result::Result<Bytes, Bytes>> = Vec::with_capacity(calls.len());
    for chunk in calls.chunks(MULTICALL_CHUNK) {
        let call3s: Vec<IMulticall3::Call3> = chunk
            .iter()
            .map(|(target, data)| IMulticall3::Call3 {
                target: *target,
                allowFailure: true,
                callData: data.clone(),
            })
            .collect();
        let calldata: Bytes = IMulticall3::aggregate3Call { calls: call3s }
            .abi_encode()
            .into();
        let tx = TransactionRequest::default()
            .to(MULTICALL3_ADDRESS)
            .input(calldata.into());
        // RPC-level errors (429 throttling, timeouts, transport) are
        // propagated: falling back to a per-call batch here would fire a
        // much larger metered burst at exactly the moment the provider is
        // failing, and callers' backoff logic would never see the error.
        let raw = provider.call(tx).block(block).await?;
        let results = match IMulticall3::aggregate3Call::abi_decode_returns(&raw) {
            Ok(r) => r,
            // A successful call with undecodable output means the address
            // holds no Multicall3 code on this chain. Batch-execute only the
            // chunks not already done — earlier chunks succeeded and must
            // not be re-executed.
            Err(_) => {
                let rest = batch_eth_calls(provider, &calls[out.len()..], block).await?;
                out.extend(rest);
                return Ok(out);
            }
        };
        for r in results {
            out.push(if r.success {
                Ok(r.returnData)
            } else {
                Err(r.returnData)
            });
        }
    }
    Ok(out)
}

/// JSON-RPC batch path for chains without Multicall3 (see
/// [`run_eth_calls`]): one HTTP request, but the provider still meters
/// every sub-call individually.
async fn batch_eth_calls<P: Provider>(
    provider: &P,
    calls: &[(Address, Bytes)],
    block: alloy::eips::BlockId,
) -> Result<Vec<std::result::Result<Bytes, Bytes>>> {
    let mut batch = alloy::rpc::client::BatchRequest::new(provider.client());
    let mut waiters = Vec::with_capacity(calls.len());
    for (target, data) in calls {
        let tx = TransactionRequest::default()
            .to(*target)
            .input(data.clone().into());
        waiters.push(
            batch
                .add_call::<_, Bytes>("eth_call", &(tx, block))
                .map_err(eyre::Error::from)?,
        );
    }
    batch.send().await.map_err(eyre::Error::from)?;
    let mut out = Vec::with_capacity(waiters.len());
    for w in waiters {
        // Propagate per-call RPC errors (429s surface here) instead of
        // degrading them to per-call failures that look like reverts —
        // the caller's backoff must see throttling to react to it.
        out.push(Ok(w.await?));
    }
    Ok(out)
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
/// Calldata for a QuoterV2/Slipstream `quoteExactInputSingle` eth_call.
fn quote_calldata(req: &QuoteRequest) -> Bytes {
    if req.slipstream {
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
    }
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
    // Reserves + leg quotes ride one Multicall3 aggregate3 (a single RPC
    // request regardless of venue/size count); eth_gasPrice is not an
    // eth_call and goes alongside as its own request.
    let mut calls: Vec<(Address, Bytes)> = Vec::with_capacity(v2_venues.len() + quotes.len());
    for pair in v2_venues {
        calls.push((
            *pair,
            IUniswapV2Pair::getReservesCall {}.abi_encode().into(),
        ));
    }
    for req in quotes {
        calls.push((req.quoter, quote_calldata(req)));
    }
    let (results, gas_price) = futures::join!(
        run_eth_calls(provider, &calls, block),
        provider.get_gas_price()
    );
    let results = results?;
    let gas_price = U256::from(gas_price.map_err(eyre::Error::from)?);

    // Per-venue error handling: a single reverted call (dead/misconfigured
    // pool) yields None for that venue instead of failing the whole scan.
    let mut v2_raw = Vec::with_capacity(v2_venues.len());
    let mut v3_quotes = Vec::with_capacity(quotes.len());
    let mut outcomes = results.into_iter();
    for _ in v2_venues {
        let entry = match outcomes.next() {
            Some(Ok(raw)) => IUniswapV2Pair::getReservesCall::abi_decode_returns(&raw)
                .ok()
                .map(|r| (U256::from(r.reserve0), U256::from(r.reserve1))),
            _ => None,
        };
        v2_raw.push(entry);
    }
    for req in quotes {
        v3_quotes.push(match outcomes.next() {
            Some(Ok(raw)) => match decode_quote(&raw) {
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
            Some(Err(e)) => {
                debug!(
                    token_in = %req.token_in,
                    token_out = %req.token_out,
                    fee_tier = req.fee_tier,
                    amount_in = %req.amount_in,
                    error = %alloy::hex::encode(&e),
                    "V3 quote reverted"
                );
                None
            }
            None => None,
        });
    }

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
    let calls: Vec<(Address, Bytes)> = requests
        .iter()
        .map(|r| (r.quoter, quote_calldata(r)))
        .collect();
    let results = run_eth_calls(provider, &calls, block).await?;
    let mut out = Vec::with_capacity(requests.len());
    for res in results {
        out.push(match res {
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
