use alloy::primitives::{Address, U256};
use clap::{Parser, Subcommand};
use eyre::{eyre, Result};
use morpho_arbitrage_bot::arbitrage::{find_opportunity, v2_quotes, VenueQuotes};
use morpho_arbitrage_bot::config::{Config, VenueKind};
use morpho_arbitrage_bot::dex::{
    fetch_cl_pair_tokens, fetch_pair_tokens, fetch_quotes, fetch_scan_snapshot,
    fetch_v3_pair_tokens, orient_reserves, probe_flashblocks_ws, read_block_id,
    PairTokens, QuoteRequest,
};
use morpho_arbitrage_bot::cl_math::cl_quote_exact_in;
use morpho_arbitrage_bot::executor;
use morpho_arbitrage_bot::sim::SimOutcome;
use morpho_arbitrage_bot::state::{self, bootstrap_cl, PoolState, StateStore};
use std::sync::Arc;
use tracing::{debug, info, warn};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

#[derive(Parser)]
#[command(
    name = "morpho-arbitrage-bot",
    about = "Morpho flashloan arbitrage bot"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan once and exit.
    Once,
    /// Continuously scan on a poll interval.
    Scan,
}

/// Immutable per-venue metadata resolved once at startup, so per-scan RPC
/// traffic is only getReserves / QuoterV2 / gasPrice (token0/token1 and the
/// contract owner never change).
struct VenueCache {
    /// (token0, token1) per venue, aligned with cfg.venues.
    pair_tokens: Vec<PairTokens>,
    /// V2/Aero venue indices and their pair addresses, for the snapshot batch.
    v2_idx: Vec<usize>,
    v2_pairs: Vec<Address>,
    /// V3 venue indices and their pool addresses (priced via QuoterV2 when
    /// uncached, or via local cl_math when bootstrap state is present).
    v3_idx: Vec<usize>,
    v3_pairs: Vec<Address>,
    /// All resolved pool addresses (V2 + V3), for the event filter.
    pool_addrs: Vec<Address>,
    /// Event-driven pool-state cache, bootstrapped at startup; CL venues
    /// present here are priced locally on every scan.
    state: StateStore,
    /// Contract owner, used as `from` in simulations/gas estimates.
    owner: Address,
    /// Startup-probed, cached Flashblock capability: `true` only when the
    /// endpoint actually streams Flashblock preconfirmations. Probed once
    /// here (WS `newFlashblocks` subscription if a pubsub provider, else the
    /// `pending`-vs-`latest` HTTP heuristic) so the per-scan path makes no
    /// extra RPC calls. When `false`, all Flashblock layers fall back to
    /// sealed-block behavior regardless of the requested env flags.
    flashblocks_available: bool,
}

impl VenueCache {
    async fn build<P: alloy::providers::Provider>(provider: &P, cfg: &Config) -> Result<Self> {
        let mut pair_tokens = Vec::with_capacity(cfg.venues.len());
        let mut v2_idx = Vec::new();
        let mut v2_pairs = Vec::new();
        let mut v3_idx = Vec::new();
        let mut v3_pairs = Vec::new();
        let mut pool_addrs = Vec::with_capacity(cfg.venues.len());
        for (idx, venue) in cfg.venues.iter().enumerate() {
            // Auto-resolve the pool from the venue's factory when the
            // config says "auto" (pair = Address::ZERO).
            let pool = if venue.pair == Address::ZERO {
                let pool = morpho_arbitrage_bot::dex::resolve_pool(
                    provider,
                    &morpho_arbitrage_bot::dex::PoolQuery {
                        kind: venue.kind,
                        factory: venue.factory,
                        router: venue.router,
                        token_a: cfg.loan_token,
                        token_b: cfg.quote_token,
                        stable: venue.stable,
                        fee_tier: venue.fee_tier,
                    },
                )
                .await?;
                info!(venue = idx, pool = %pool, kind = ?venue.kind, "pool auto-resolved");
                pool
            } else {
                venue.pair
            };
            let tokens = if venue.kind == VenueKind::UniswapV3 {
                v3_idx.push(idx);
                v3_pairs.push(pool);
                fetch_v3_pair_tokens(provider, pool).await?
            } else if venue.kind == VenueKind::Slipstream {
                // Slipstream CL pools are priced via their own quoter; from the
                // scanner's perspective they behave like V3.
                v3_idx.push(idx);
                v3_pairs.push(pool);
                fetch_cl_pair_tokens(provider, pool).await?
            } else {
                v2_idx.push(idx);
                v2_pairs.push(pool);
                fetch_pair_tokens(provider, pool).await?
            };
            // Fail fast on misconfigured venues: both cycle tokens must be
            // in the pair, otherwise every scan would silently skip it.
            for (label, token) in [("loan", cfg.loan_token), ("quote", cfg.quote_token)] {
                if token != tokens.token0 && token != tokens.token1 {
                    eyre::bail!("venue {idx} pool {pool} does not contain {label} token {token}");
                }
            }
            pair_tokens.push(tokens);
            pool_addrs.push(pool);
        }
        let owner = executor::fetch_owner(provider, cfg.arb_contract).await?;
        // Probe Flashblock capability once, at startup, using ONLY the
        // Flashblock-specific `newFlashblocks` WebSocket subscription. The
        // `pending`-vs-`latest` block-number heuristic is deliberately NOT
        // used: ordinary Ethereum nodes number their pending candidate
        // `latest + 1` and would be misclassified as Flashblock-aware,
        // bypassing the sealed-state fallback and exposing scans to mutable
        // state. A Flashblock-aware WSS endpoint is required to enable any
        // pending layer; without one, every layer stays on sealed behavior.
        // The result is cached so the per-scan path never re-probes.
        let flashblocks_available = if cfg.flashblocks_enabled() {
            probe_flashblocks_via_ws(cfg).await
        } else {
            false
        };
        // Bootstrap every resolved CL pool into the local state store; a
        // failed bootstrap just keeps that venue on the QuoterV2 fallback.
        let mut state = StateStore::new();
        for pool in v3_pairs.iter() {
            match bootstrap_cl(provider, *pool).await {
                Ok(ps) => {
                    let n_ticks = match &ps {
                        PoolState::Cl { ticks, .. } => ticks.len(),
                        PoolState::V2 { .. } => 0,
                    };
                    info!(pool = %pool, ticks = n_ticks, "CL pool bootstrapped into local state");
                    state.insert(*pool, ps);
                }
                Err(e) => warn!(pool = %pool, error = %e, "CL bootstrap failed; using QuoterV2 fallback"),
            }
        }
        Ok(Self {
            pair_tokens,
            v2_idx,
            v2_pairs,
            v3_idx,
            v3_pairs,
            pool_addrs,
            state,
            owner,
            flashblocks_available,
        })
    }
}

/// Probe Flashblock support by attempting the Flashblock-specific
/// `newFlashblocks` subscription against the configured WebSocket endpoint
/// (`WSS_URL`). This subscription is only implemented by Flashblock-aware
/// nodes (absent from stock OP-Stack), so a successful subscribe is a strong,
/// Flashblock-specific signal — unlike the `pending > latest` block-number
/// heuristic, which ordinary nodes also satisfy. Requires a WSS endpoint; an
/// HTTP-only config returns `false` (sealed-block behavior) rather than
/// guessing, because inferring Flashblock support from `pending` numbers is
/// unreliable and would silently enable mutable-state reads on plain nodes.
async fn probe_flashblocks_via_ws(cfg: &Config) -> bool {
    let Some(url) = &cfg.wss_url else {
        warn!(
            "FLASHBLOCKS enabled but no WSS_URL: Flashblock-specific probe \
             requires a WebSocket endpoint; falling back to sealed-block behavior"
        );
        return false;
    };
    let client = match alloy::rpc::client::RpcClient::connect_pubsub(
        alloy::rpc::client::WsConnect::new(url.clone()),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                wss = %redact_url(url),
                error = %e,
                "Flashblock WS probe failed to connect; falling back to sealed-block behavior"
            );
            return false;
        }
    };
    let provider =
        alloy::providers::RootProvider::<alloy::network::Ethereum>::new(client);
    let available = probe_flashblocks_ws(&provider).await;
    debug!(
        wss = %redact_url(url),
        flashblocks_available = available,
        "Flashblock probe complete (newFlashblocks subscribe accepted = capability)"
    );
    available
}

#[tokio::main]
async fn main() -> Result<()> {
    // The default directive must be set via the builder: `add_directive` on a
    // parsed filter replaces any same-specificity directive from RUST_LOG, so
    // `RUST_LOG=debug` would be silently overwritten by the `info` fallback.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    // Broadcast provider with the signing wallet attached, built once and
    // reused across scans (executor no longer opens a fresh connection per
    // trade).
    let broadcaster = build_broadcaster(&cfg)?;

    // One-off startup resolution: pair tokens for orientation/validation and
    // the contract owner for simulations. ~3 RPC calls per venue, once.
    let mut cache = VenueCache::build(&broadcaster, &cfg).await?;

    info!(
        morpho = %cfg.morpho,
        arb_contract = %cfg.arb_contract,
        owner = %cache.owner,
        loan_token = %cfg.loan_token,
        quote_token = %cfg.quote_token,
        venues = cfg.venues.len(),
        loan_amounts = ?cfg.loan_amounts,
        dry_run = cfg.dry_run,
        flashblocks_available = cache.flashblocks_available,
        pending_state = cfg.use_pending_state,
        flashblock_sync = cfg.use_flashblock_sync,
        pending_logs = cfg.use_pending_logs,
        pending_sim = cfg.use_pending_sim,
        "bot configured (effective Flashblock flags reflect the startup probe; \
         layers whose RPC is unavailable auto-fall back to sealed-block behavior)"
    );
    for (i, v) in cfg.venues.iter().enumerate() {
        info!(
            venue = i,
            pool = %cache.pool_addrs[i],
            configured = %v.pair,
            kind = ?v.kind,
            fee_bps = v.fee_bps,
            fee_tier = v.fee_tier,
            "configured venue"
        );
    }

    match cli.command {
        Command::Once => run_once(&cfg, &cache, &broadcaster, None).await?,
        Command::Scan => {
            let inflight = Arc::new(std::sync::atomic::AtomicBool::new(false));
            if let Some(wss_url) = &cfg.wss_url {
                info!(wss = %redact_url(wss_url), "starting event-driven scanning via WebSocket");
                run_event_driven(&cfg, &mut cache, wss_url, &broadcaster, &inflight).await?;
            } else {
                info!(
                    poll_ms = cfg.poll_interval_ms,
                    "starting polling-based scanning"
                );
                loop {
                    if let Err(e) = run_once(&cfg, &cache, &broadcaster, Some(&inflight)).await {
                        warn!(error = %e, "scan iteration failed");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms))
                        .await;
                }
            }
        }
    }
    Ok(())
}

/// Pick the quoter for a V3 venue: its per-venue override when set,
/// otherwise the global config quoter (QUOTER_V2 for Uniswap-style V3,
/// QUOTER_SLIPSTREAM for Aerodrome CL).
fn resolve_quoter(cfg: &Config, venue: &morpho_arbitrage_bot::config::Venue) -> Address {
    if venue.quoter != Address::ZERO {
        return venue.quoter;
    }
    if venue.kind == VenueKind::Slipstream {
        return cfg.quoter_slipstream;
    }
    cfg.quoter_v2
}

/// Strip credentials from a URL for logging: keep scheme + host, drop the
/// path/query where API keys typically live (e.g. Chainstack endpoints).
fn redact_url(url: &str) -> String {
    match url.find("://") {
        Some(i) => {
            let rest = &url[i + 3..];
            let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            format!("{}://{}", &url[..i], host)
        }
        None => url.split(['/', '?', '#']).next().unwrap_or(url).to_string(),
    }
}

/// Build the wallet-enabled HTTP provider used to broadcast trades.
fn build_broadcaster(cfg: &Config) -> Result<impl alloy::providers::Provider + Clone + 'static> {
    use alloy::network::EthereumWallet;
    use alloy::signers::local::PrivateKeySigner;

    let signer: PrivateKeySigner = cfg.private_key.parse()?;
    let wallet = EthereumWallet::from(signer);
    Ok(alloy::providers::ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(cfg.rpc_url.parse()?))
}

/// Topic-0 signatures of pool events that can move the price of a watched
/// pool: V2 Sync/Swap/Mint/Burn, V3 Swap/Mint/Burn, plus the Aerodrome
/// (Velodrome fork) variants, whose Sync/Swap/Burn declarations differ
/// from Uniswap V2 and therefore hash to different topic0 values.
/// Aerodrome Slipstream (CL) is a UniV3 fork with identical event
/// signatures, so its Swap/Mint/Burn topic0s are already covered here.
fn pool_event_signatures() -> Vec<alloy::primitives::B256> {
    [
        // Uniswap V2 (also Sushiswap/Pancakeswap V2).
        "Sync(uint112,uint112)",
        "Swap(address,uint256,uint256,uint256,uint256,address)",
        "Mint(address,uint256,uint256)",
        "Burn(address,uint256,uint256,address)",
        // Aerodrome vAMM: Sync/Swap use uint256 and Burn orders `to` before
        // the amounts, so all three hash differently from the V2 originals.
        "Sync(uint256,uint256)",
        "Swap(address,address,uint256,uint256,uint256,uint256)",
        "Burn(address,address,uint256,uint256)",
        // Uniswap V3 (also Aerodrome Slipstream CL pools).
        "Swap(address,address,int256,int256,uint160,uint128,int24)",
        "Mint(address,address,int24,int24,uint128,uint256,uint256)",
        "Burn(address,int24,int24,uint128,uint256,uint256)",
    ]
    .into_iter()
    .map(alloy::primitives::keccak256)
    .collect()
}

/// Event-driven scanning: a scan is triggered by a price-moving event on any
/// watched pool (eth_logs subscription), with a full sweep forced every
/// `cfg.sweep_interval_blocks` blocks as a safety net against missed events.
/// newHeads drives the sweep clock; several pool events in one block still
/// cause only one scan, since chain reads are pinned to the sealed latest
/// block anyway. On subscription failure or stream end, falls back to
/// polling so the bot keeps running (a dropped WSS connection must not kill
/// the process).
async fn run_event_driven<B>(
    cfg: &Config,
    cache: &mut VenueCache,
    wss_url: &str,
    broadcaster: &B,
    inflight: &InflightFlag,
) -> Result<()>
where
    B: alloy::providers::Provider + Clone + 'static,
{
    use alloy::providers::Provider;
    use futures::StreamExt;

    let ws = alloy::rpc::client::WsConnect::new(wss_url);
    let client = alloy::rpc::client::RpcClient::connect_pubsub(ws).await?;
    let provider = alloy::providers::RootProvider::<alloy::network::Ethereum>::new(client);

    let heads_sub = provider.subscribe_blocks().await?;
    let mut heads = heads_sub.into_stream();

    // Without the log subscription (provider rejects the filter, etc.) the
    // sweep interval becomes 1 block, reproducing scan-per-block behavior.
    let base_filter = alloy::rpc::types::Filter::new()
        .address(cache.pool_addrs.clone())
        .event_signature(pool_event_signatures());
    let mut sweep_every = cfg.sweep_interval_blocks;

    // Flashblock preconfirmed logs: when enabled, subscribe to Base's
    // non-standard `pendingLogs` subscription type. Crucially this must be a
    // real `pendingLogs` subscription, not a `logs` filter with pending block
    // bounds (that still streams sealed-block logs). A non-Flashblock endpoint
    // rejects `pendingLogs`, in which case the sealed-block `logs` stream
    // below still drives scans.
    let mut pending_logs = if cfg.use_pending_logs {
        match subscribe_pending_logs(&provider, &base_filter).await {
            Ok(sub) => {
                info!(
                    pools = cache.pool_addrs.len(),
                    "subscribed to preconfirmed (Flashblock) pool logs; \
                     scanning on 200ms events"
                );
                Some(sub.into_stream().boxed())
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "pendingLogs subscription failed; \
                     falling back to sealed-block pool events"
                );
                None
            }
        }
    } else {
        None
    };
    let mut pending_available = pending_logs.is_some();

    let mut logs = match provider.subscribe_logs(&base_filter).await {
        Ok(sub) => {
            info!(
                pools = cache.pool_addrs.len(),
                sweep_every, "subscribed to newHeads + pool logs; scanning on pool events"
            );
            sub.into_stream().boxed()
        }
        Err(e) => {
            warn!(error = %e, "log subscription failed; scanning every block");
            sweep_every = 1;
            futures::stream::pending().boxed()
        }
    };

    // Block number of the last scan; any scan (event- or sweep-triggered)
    // resets the sweep clock because both run the same full scan.
    let mut last_scanned = 0u64;
    // Wall-clock start of the last scan; enforces the RPS-protecting
    // minimum gap between scans when configured.
    let mut last_scan_at = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(60))
        .unwrap_or_else(std::time::Instant::now);
    let min_gap = std::time::Duration::from_millis(cfg.min_scan_interval_ms);
    loop {
        let trigger = tokio::select! {
            header = heads.next() => {
                let Some(header) = header else { break };
                let block = header.number;
                if block >= last_scanned + sweep_every {
                    Some(("sweep", block))
                } else {
                    None
                }
            }
            log = logs.next() => {
                match log {
                    // Degraded mode: fall back to scanning every block.
                    None => {
                        warn!("log subscription ended; scanning every block");
                        sweep_every = 1;
                        logs = futures::stream::pending().boxed();
                        None
                    }
                    // Reorged-away events must not trigger a scan.
                    Some(log) if log.removed => None,
                    Some(log) => {
                        // Fold the event into the local pool state first so
                        // the triggered scan prices against fresh inventory.
                        apply_pool_log(&mut *cache, &log);
                        match log.block_number {
                            // Multiple pool events within one block coalesce into
                            // a single scan: reads are pinned to the sealed
                            // latest block, so later events add nothing.
                            Some(block) if block > last_scanned => Some(("pool event", block)),
                            _ => None,
                        }
                    }
                }
            }
            // Preconfirmed (Flashblock) pool logs: fire ~200ms after the
            // event, well before the sealed block. These can reorg against
            // the final block, so drop reorged entries and let the sealed
            // stream + sweep interval act as the backstop. The trigger
            // block is the current latest, since the preconfirmed log's
            // block number is the in-progress block; the scan reads fresh
            // state regardless.
            log = async {
                match &mut pending_logs {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            }, if pending_available => {
                match log {
                    // Stream ended: disable this branch permanently and
                    // degrade to sealed logs + sweeps. Keeping the branch
                    // enabled would spin: `next()` resolves immediately with
                    // None forever.
                    None => {
                        warn!("pendingLogs stream ended; falling back to sealed logs");
                        pending_available = false;
                        pending_logs = None;
                        None
                    }
                    Some(log) if log.removed => None,
                    Some(log) => {
                        // Same as the sealed branch: apply into local state
                        // before scanning, so the scan reads fresh inventory.
                        apply_pool_log(&mut *cache, &log);
                        // Use the latest sealed block as the watermark so
                        // the sweep clock advances; the actual scan reads
                        // pending state when USE_PENDING_STATE is on.
                        let block = last_scanned;
                        Some(("flashblock event", block))
                    }
                }
            }
        };
        let Some((reason, block)) = trigger else {
            continue;
        };
        // Rate-limit scans: drop triggers arriving inside the cooldown
        // window instead of queueing them — by the next scan, `latest`
        // already includes their state changes.
        if last_scan_at.elapsed() < min_gap {
            continue;
        }
        last_scan_at = std::time::Instant::now();
        info!(block, reason, "scanning");
        match run_once_with_provider(cfg, cache, &provider, broadcaster, Some(inflight)).await {
            // Advance by the block the scan actually read (latest at scan
            // time), not the trigger block, so buffered events for blocks
            // already covered by that read don't fire redundant scans.
            Ok(scanned) => last_scanned = last_scanned.max(scanned),
            Err(e) => warn!(error = %e, "event-driven scan failed"),
        }
    }
    warn!("block subscription ended; falling back to polling");
    loop {
        if let Err(e) = run_once(cfg, cache, broadcaster, Some(inflight)).await {
            warn!(error = %e, "scan iteration failed");
        }
        tokio::time::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms)).await;
    }
}

/// Shared "a trade is in flight" flag. Because broadcasting is
/// fire-and-forget, the next scan would re-detect the same opportunity
/// (prices are unchanged until the pending tx is included) and broadcast a
/// competing duplicate that burns gas on revert. The flag is cleared by the
/// background receipt watcher once the tx is included.
type InflightFlag = Arc<std::sync::atomic::AtomicBool>;

/// Look up a CL venue's bootstrapped state by real pool address; V2 pool
/// lookups (Address::ZERO) yield None so the caller falls back to RPC.
fn cached_cl(cache: &VenueCache, pool: Address) -> Option<&PoolState> {
    if pool == Address::ZERO {
        return None;
    }
    match cache.state.get(&pool) {
        Some(PoolState::Cl { .. }) => cache.state.get(&pool),
        _ => None,
    }
}

/// Local CL quote for one exact-input swap; delegates to cl_math.
fn cl_quote(amount_in: U256, state: &PoolState, zero_for_one: bool) -> Option<U256> {
    cl_quote_exact_in(state, zero_for_one, amount_in)
}

/// Kind-tagged pool event hashes, computed once by the callers at the top
/// of each classification (keccak of the Solidity signature). Only the
/// payload-relevant events need decoding semantics here.
fn v2_sync_hash() -> alloy::primitives::B256 {
    alloy::primitives::keccak256("Sync(uint112,uint112)")
}

/// CL Swap/Mint/Burn identically agree between Uniswap V3 and Slipstream.
fn cl_swap_hash() -> alloy::primitives::B256 {
    alloy::primitives::keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)")
}
fn cl_mint_hash() -> alloy::primitives::B256 {
    alloy::primitives::keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)")
}
fn cl_burn_hash() -> alloy::primitives::B256 {
    alloy::primitives::keccak256("Burn(address,int24,int24,uint128,uint256,uint256)")
}

/// Fold one pool log into the local state store. Only Sync/CL-Swap/
/// Mint/Burn events change price; anything else is skipped silently.
/// Malformed payloads are ignored — one bad log never crashes the loop.
fn apply_pool_log(cache: &mut VenueCache, log: &alloy::rpc::types::eth::Log) {
    let pool = log.address();
    let topics = log.topics();
    let data = log.data();
    let data: &[u8] = data.data.as_ref();
    let Some(&topic0) = topics.first() else {
        return;
    };
    let v2_sync = v2_sync_hash();
    let cl_swap = cl_swap_hash();
    let cl_mint = cl_mint_hash();
    let cl_burn = cl_burn_hash();
    if topic0 == v2_sync {
        if let Some(ev) = state::decode_v2_sync(data) {
            debug!(pool = %pool, kind = "sync", "applied pool log to state store");
            cache.state.apply_v2_sync(pool, ev);
        }
    } else if topic0 == cl_swap {
        if let Some(ev) = state::decode_cl_swap(data) {
            cache.state.apply_cl_swap(pool, ev);
        }
    } else if topic0 == cl_mint || topic0 == cl_burn {
        let is_burn = topic0 == cl_burn;
        if let Some(ev) = state::decode_cl_liquidity(data, topics, is_burn) {
            cache.state.apply_cl_liquidity(pool, ev, is_burn);
        }
    }
}


/// Subscribe to Base's non-standard `pendingLogs` subscription: emits the
/// logs of transactions as they are pre-confirmed in Flashblocks (~200ms),
/// well before the sealed block. This is distinct from `eth_subscribe "logs"`
/// with a pending block filter, which still yields sealed-block logs. The
/// address/topic filter is passed as the second subscribe parameter so we
/// only get logs from watched pools.
///
/// Returns the raw subscription stream of `Log`; the caller drops reorged
/// (`removed`) entries and falls back to sealed logs if this fails.
async fn subscribe_pending_logs<P: alloy::providers::Provider>(
    provider: &P,
    filter: &alloy::rpc::types::Filter,
) -> eyre::Result<alloy::pubsub::Subscription<alloy::rpc::types::eth::Log>>
where
    P: 'static,
{
    // eth_subscribe("pendingLogs", filter) — params serialize as the
    // 2-element array Base expects: [subscription-kind, filter-object].
    let params = ("pendingLogs", filter.clone());
    let sub = provider
        .subscribe::<(&str, alloy::rpc::types::Filter), alloy::rpc::types::eth::Log>(params)
        .await?;
    Ok(sub)
}

/// Run one scan iteration with a given provider. Chain reads happen in two
/// JSON-RPC batches, both pinned to the same block: phase 1 fetches V2
/// reserves, V3 leg-1 quotes (one QuoterV2 call per venue x loan size) and
/// the gas price; phase 2 quotes V3 leg 2, whose inputs are only known once
/// leg 1 has been priced. Pinning both phases to one block keeps the two
/// legs of a cycle priced against a consistent chain state. Returns the
/// block number the reads were pinned to, so the event loop can track
/// which chain state has actually been covered.
async fn run_once_with_provider<P, B>(
    cfg: &Config,
    cache: &VenueCache,
    provider: &P,
    broadcaster: &B,
    inflight: Option<&InflightFlag>,
) -> Result<u64>
where
    P: alloy::providers::Provider + Clone,
    B: alloy::providers::Provider + Clone + 'static,
{
    let sizes = &cfg.loan_amounts;

    // Pin both phases to one block so the legs of a cycle are priced
    // against the same chain state. When Flashblock preconfirmed state is
    // enabled and the endpoint streams it, this is the ~200ms-fresh `pending`
    // tag; otherwise a sealed `latest` block number. The cached startup
    // capability (`cache.flashblocks_available`) decides pending vs sealed,
    // so the per-scan path makes no extra probe RPC.
    let want_pending = cfg.use_pending_state && cache.flashblocks_available;
    let block = read_block_id(provider, want_pending, Some(cache.flashblocks_available)).await?;
    // The `pending` tag is mutable — Base advances it ~every 200ms as new
    // Flashblocks land. Snapshot its current sealed block number now so we can
    // detect, before broadcasting, that a fresh Flashblock rewrote the state
    // the scan read (mixed-state legs would revert and waste gas).
    let scan_block_number = if want_pending {
        provider.get_block_number().await?
    } else {
        0
    };
    // Track the sealed block for sweep bookkeeping; in pending mode the
    // preconfirmed state maps to the in-progress sealed block, so
    // get_block_number (latest) is the correct watermark.
    let block_number = provider.get_block_number().await?;
    debug!(
        block = ?block,
        block_number,
        scan_block_number,
        want_pending,
        flashblocks_available = cache.flashblocks_available,
        pending_state = cfg.use_pending_state,
        "scan pinned to block"
    );

    // Phase 1 batch: reserves + leg-1 quotes (loan -> quote) + gas price.
    // CL venues with a bootstrapped PoolState are priced locally and
    // excluded from this RPC batch; the rest still ride QuoterV2.
    let mut leg1_requests = Vec::new();
    for (j, &idx) in cache.v3_idx.iter().enumerate() {
        if cached_cl(cache, cache.v3_pairs[j]).is_some() {
            continue;
        }
        let venue = &cfg.venues[idx];
        let slipstream = venue.kind == VenueKind::Slipstream;
        for &size in sizes {
            leg1_requests.push(QuoteRequest {
                token_in: cfg.loan_token,
                token_out: cfg.quote_token,
                fee_tier: venue.fee_tier,
                amount_in: size,
                quoter: resolve_quoter(cfg, venue),
                slipstream,
            });
        }
    }
    let snapshot = fetch_scan_snapshot(provider, &cache.v2_pairs, &leg1_requests, block).await?;
    let gas_price = cfg.gas_price_wei.unwrap_or(snapshot.gas_price);

    // Assemble leg-1 outputs per venue; V3 quotes come straight from the
    // snapshot, V2 outputs are exact constant-product math on reserves.
    // Each entry keeps its reserves for the local leg-2 computation below.
    struct Leg1 {
        quotes: VenueQuotes,
        v2_reserves: Option<morpho_arbitrage_bot::dex::PoolReserves>,
    }
    let mut legs: Vec<Leg1> = Vec::with_capacity(cfg.venues.len());
    for (j, &idx) in cache.v2_idx.iter().enumerate() {
        let Some((r0, r1)) = snapshot.v2_raw[j] else {
            warn!(venue = idx, pair = %cfg.venues[idx].pair, "reserve fetch reverted; skipping venue");
            continue;
        };
        let venue = &cfg.venues[idx];
        let reserves =
            match orient_reserves(r0, r1, &cache.pair_tokens[idx], venue.pair, cfg.loan_token) {
                Ok(r) => r,
                Err(e) => {
                    warn!(venue = idx, error = %e, "skipping venue");
                    continue;
                }
            };
        legs.push(Leg1 {
            quotes: v2_quotes(idx, reserves, venue.fee_bps, sizes, &[]),
            v2_reserves: Some(reserves),
        });
    }
    let n_sizes = sizes.len();
    let mut next_unchecked = 0usize; // request index among uncached v3 venues
    for (j, &idx) in cache.v3_idx.iter().enumerate() {
        let pool = cache.v3_pairs[j];
        if let Some(ps) = cached_cl(cache, pool) {
            // Local CL quote path — identical math to the venue's quoter,
            // validated against on-chain Quoter in tests/chain_cl.rs.
            let zero_for_one = cache.pair_tokens[idx].token0 == cfg.loan_token;
            let lo = ps;
            let leg1: Vec<Option<U256>> = sizes
                .iter()
                .map(|&s| cl_quote(s, lo, zero_for_one))
                .collect();
            if leg1.iter().all(|q| q.is_none()) {
                warn!(venue = idx, pool = %pool, "cached CL pool unusable; skipping venue");
                continue;
            }
            legs.push(Leg1 {
                quotes: VenueQuotes { venue: idx, leg1, leg2: Vec::new() },
                v2_reserves: None,
            });
            continue;
        }
        let base = next_unchecked;
        next_unchecked += n_sizes;
        let leg1: Vec<Option<U256>> = snapshot.v3_quotes[base..base + n_sizes].to_vec();
        if leg1.iter().all(|q| q.is_none()) {
            let label = if cfg.venues[idx].kind == VenueKind::Slipstream {
                "Slipstream"
            } else {
                "V3"
            };
            warn!(
                venue = idx,
                "{label} venue returned no usable quotes; skipping venue"
            );
            continue;
        }
        legs.push(Leg1 {
            quotes: VenueQuotes {
                venue: idx,
                leg1,
                leg2: Vec::new(),
            },
            v2_reserves: None,
        });
    }

    // Phase 2: leg 2 (quote -> loan) for every distinct leg-1 output of the
    // OTHER venues. V2 legs are exact local math; V3 legs go through one
    // more QuoterV2 batch.
    let mut phase2: Vec<(usize, QuoteRequest)> = Vec::new(); // (position in legs, request)
    for s in 0..legs.len() {
        let mut inputs: Vec<U256> = Vec::new();
        for (f, other) in legs.iter().enumerate() {
            if f == s {
                continue;
            }
            for q in other.quotes.leg1.iter().flatten() {
                if !inputs.contains(q) {
                    inputs.push(*q);
                }
            }
        }
        let venue_idx = legs[s].quotes.venue;
        let venue = &cfg.venues[venue_idx];
        // V3-index lookup; V2 pool lookups map to Address::ZERO so cached_cl
        // finds nothing and keeps the fallthrough RPC behavior.
        let v3_pos = cache
            .v3_idx
            .iter()
            .position(|&i| i == venue_idx)
            .unwrap_or(usize::MAX);
        let pool = if v3_pos == usize::MAX {
            Address::ZERO
        } else {
            cache.v3_pairs[v3_pos]
        };
        if let Some(reserves) = legs[s].v2_reserves {
            legs[s].quotes.leg2 = v2_quotes(
                venue_idx,
                reserves,
                venue.fee_bps,
                &[],
                &inputs,
            )
            .leg2;
        } else if let Some(ps) = cached_cl(cache, pool) {
            // Local CL path for leg 2 (quote -> loan).
            let zero_for_one = cache.pair_tokens[venue_idx].token0 == cfg.quote_token;
            legs[s].quotes.leg2 = inputs
                .iter()
                .map(|&q| (q, cl_quote(q, ps, zero_for_one)))
                .collect();
        } else {
            let slipstream = venue.kind == VenueKind::Slipstream;
            for q in inputs {
                phase2.push((
                    s,
                    QuoteRequest {
                        token_in: cfg.quote_token,
                        token_out: cfg.loan_token,
                        fee_tier: venue.fee_tier,
                        amount_in: q,
                        quoter: resolve_quoter(cfg, venue),
                        slipstream,
                    },
                ));
            }
        }
    }
    if !phase2.is_empty() {
        let requests: Vec<QuoteRequest> = phase2.iter().map(|(_, r)| *r).collect();
        let results = fetch_quotes(provider, &requests, block).await?;
        let mut grouped: Vec<Vec<(U256, Option<U256>)>> =
            (0..legs.len()).map(|_| Vec::new()).collect();
        for ((s, req), out) in phase2.iter().zip(results) {
            grouped[*s].push((req.amount_in, out));
        }
        for (s, leg2) in grouped.into_iter().enumerate() {
            if !leg2.is_empty() {
                legs[s].quotes.leg2 = leg2;
            }
        }
    }

    let quotes: Vec<VenueQuotes> = legs.into_iter().map(|l| l.quotes).collect();

    for (i, q) in quotes.iter().enumerate() {
        debug!(
            venue = i,
            leg1 = ?q.leg1,
            leg2 = ?q.leg2,
            "venue quotes"
        );
    }

    // Phase-1 → phase-2 state-advancement guard. When scanning preconfirmed
    // `pending` state, the tag is mutable — Base advances it ~every 200ms. If
    // a new Flashblock landed between the phase-1 batch (reserves/leg-1
    // quotes) and the phase-2 batch, leg-1 and leg-2 are priced against two
    // different states, so the computed spread is invalid and any trade built
    // on it would likely revert. Detect the advancement by re-checking the
    // sealed block number; if it moved, discard the whole scan and let the
    // caller rescan rather than act on mixed-state legs.
    if want_pending {
        let now = provider.get_block_number().await?;
        if now != scan_block_number {
            info!(
                scan_block = scan_block_number,
                now_block = now,
                "pending state advanced between phase 1 and phase 2; discarding mixed-state scan"
            );
            return Ok(block_number);
        }
    }

    let Some(opp) = find_opportunity(sizes, &quotes, cfg.min_profit) else {
        info!("no profitable opportunity");
        return Ok(block_number);
    };

    // Pre-broadcast state-advancement guard (second check). Even after the
    // phase-1→phase-2 check above, a Flashblock may land between then and the
    // broadcast, so the priced state no longer matches what we'd submit
    // against. Re-check; if it advanced, discard and rescan.
    if want_pending {
        let now = provider.get_block_number().await?;
        if now != scan_block_number {
            info!(
                scan_block = scan_block_number,
                now_block = now,
                "pending state advanced during scan; discarding to avoid mixed-state legs"
            );
            return Ok(block_number);
        }
    }

    // Estimate gas cost and subtract from profit. Gas is paid in ETH;
    // config enforces loan_token == wrapped_native, so the wei estimate
    // is directly comparable to profit in loan-token units.
    //
    // In dry-run, apply a constant gas-units estimate instead of skipping
    // the cost entirely: with gas=0 the MIN_PROFIT filter runs against
    // gross profit, so dry-run reports "opportunities" that live mode
    // (which subtracts the real estimate_gas result) always rejects. The
    // constant intentionally needs no RPC call and errs high for the
    // Morpho flashloan + two router swaps path.
    let gas_cost_loan = if cfg.dry_run {
        const DRY_RUN_GAS_UNITS: u64 = 400_000;
        U256::from(DRY_RUN_GAS_UNITS) * gas_price
    } else {
        // Two-stage build: estimate gas with a provisional params
        // (minProfit barely affects calldata size/gas), then rebuild below
        // with the on-chain backstop raised to min_profit + gas so the
        // contract itself reverts net-unprofitable trades.
        //
        // estimate_gas executes the real swaps in simulation, so it
        // reverts whenever minOut is unattainable (e.g. the spread is
        // thinner than the per-leg slippage tolerance on a volatile
        // pair). That is not a scan failure — it is a per-opportunity
        // rejection, same as the net-profit filter; skip and keep
        // scanning instead of failing the whole scan.
        let provisional = executor::build_params(cfg, &opp, cfg.min_profit);
        // Gate the gas estimate against the preconfirmed `pending` state only
        // when both USE_PENDING_SIM is requested AND the endpoint actually
        // streams Flashblocks (cached capability). Otherwise the scan's block
        // id is a sealed `latest` number and "pending sim" would silently run
        // against sealed state — pointless and misleading. Falls back to the
        // node default (latest) state when not pending.
        let sim_block = if cfg.use_pending_sim && want_pending {
            Some(block)
        } else {
            None
        };
        // When USE_LOCAL_SIM is on, try the in-process revm simulation
        // first. A revert is a per-opportunity verdict (same as an
        // eth_estimateGas revert); a DB/transport error falls back to the
        // node's estimate so local sim can never strand a tradeable block.
        let local_gas = if cfg.use_local_sim {
            match executor::estimate_gas_local(
                provider.clone(),
                cfg.arb_contract,
                cache.owner,
                provisional.clone(),
                sim_block.unwrap_or(alloy::eips::BlockId::latest()),
            ) {
                Ok(SimOutcome::Success { gas_used, .. }) => {
                    debug!(gas_units = gas_used, "local revm gas estimate");
                    Some(Ok(U256::from(gas_used)))
                }
                Ok(SimOutcome::Reverted(reason)) => Some(Err(eyre!(reason))),
                Err(e) => {
                    warn!(error = %e, "local sim unavailable; falling back to eth_estimateGas");
                    None
                }
            }
        } else {
            None
        };
        match match local_gas {
            Some(outcome) => outcome,
            None => {
                executor::estimate_gas(provider, cfg.arb_contract, cache.owner, provisional, sim_block)
                    .await
            }
        } {
            Ok(gas_estimate) => {
                let gas_cost = gas_estimate * gas_price;
                debug!(
                    gas_units = ?gas_estimate,
                    gas_price = %gas_price,
                    gas_cost = %gas_cost,
                    sim_block = ?sim_block,
                    "gas estimate for opportunity"
                );
                gas_cost
            }
            Err(e) => {
                info!(error = %e, "opportunity rejected: simulated execution reverted");
                return Ok(block_number);
            }
        }
    };

    let net_profit = opp.profit.saturating_sub(gas_cost_loan);
    let onchain_min_profit = cfg.min_profit + gas_cost_loan;
    if net_profit < cfg.min_profit {
        info!(
            gross = %opp.profit,
            gas = %gas_cost_loan,
            net = %net_profit,
            "opportunity filtered out by gas cost"
        );
        return Ok(block_number);
    }

    info!(
        first = opp.first,
        second = opp.second,
        loan = %opp.loan_amount,
        gross = %opp.profit,
        gas = %gas_cost_loan,
        net = %net_profit,
        "opportunity found"
    );

    if cfg.dry_run {
        // No simulate() here: it costs one eth_call per scan and reverts
        // whenever the owner wallet holds no WETH/approval — pure noise in
        // a mode whose only purpose is to observe the scanner's verdicts.
        info!("dry-run enabled; skipping broadcast");
        return Ok(block_number);
    }

    let params = executor::build_params(cfg, &opp, onchain_min_profit);

    // Claim the in-flight slot before broadcasting so subsequent scans
    // skip trading while this tx is pending inclusion. Without this the
    // very next scan would re-detect the same opportunity and broadcast a
    // competing duplicate (distinct nonce) that only burns gas on revert.
    if let Some(flag) = inflight {
        if flag
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            info!("trade already in flight; skipping duplicate broadcast");
            return Ok(block_number);
        }
    }
    // Fire-and-forget: returns once the node accepts the tx; the receipt
    // watcher clears the in-flight flag on inclusion. When Flashblock sync
    // is enabled, the submit blocks ~200ms for a synchronous receipt
    // instead (clearing the flag ~10x sooner), but falls back to this
    // fire-and-forget path on timeout/unsupported endpoints.
    let outcome = if cfg.use_flashblock_sync {
        executor::execute_sync(
            broadcaster.clone(),
            cfg.arb_contract,
            params,
            inflight.cloned(),
        )
        .await
    } else {
        executor::execute(
            broadcaster.clone(),
            cfg.arb_contract,
            params,
            inflight.cloned(),
        )
        .await
    };
    match outcome {
        Ok(tx) => info!(tx = %tx, "arbitrage transaction broadcast"),
        Err(e) => {
            if let Some(flag) = inflight {
                flag.store(false, std::sync::atomic::Ordering::Release);
            }
            return Err(e);
        }
    }
    Ok(block_number)
}

/// Run one scan iteration (convenience wrapper for polling mode).
async fn run_once<B>(
    cfg: &Config,
    cache: &VenueCache,
    broadcaster: &B,
    inflight: Option<&InflightFlag>,
) -> Result<()>
where
    B: alloy::providers::Provider + Clone + 'static,
{
    let provider = alloy::providers::ProviderBuilder::new().connect_http(cfg.rpc_url.parse()?);
    run_once_with_provider(cfg, cache, &provider, broadcaster, inflight)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod pool_event_tests {
    use super::pool_event_signatures;
    use alloy::primitives::{b256, B256};

    #[test]
    fn signatures_match_canonical_topic0() {
        let sigs = pool_event_signatures();
        // Well-known topic0 values, cross-checked against Uniswap V2/V3
        // deployments; a typo in the signature strings would silently
        // disable event triggers.
        let v2_sync = b256!("1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1");
        let v3_swap = b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
        assert!(sigs.contains(&v2_sync));
        assert!(sigs.contains(&v3_swap));
        assert_eq!(sigs.len(), 10);
        // All distinct: a duplicate would only bloat the filter.
        let unique: std::collections::HashSet<B256> = sigs.iter().copied().collect();
        assert_eq!(unique.len(), sigs.len());
    }
}

#[cfg(test)]
mod redact_tests {
    use super::redact_url;

    #[test]
    fn strips_path_and_query() {
        assert_eq!(
            redact_url("wss://node.example.com/SECRETAPIKEY"),
            "wss://node.example.com"
        );
        assert_eq!(
            redact_url("https://eth.example.com/v2/KEY?x=1"),
            "https://eth.example.com"
        );
        assert_eq!(
            redact_url("https://mainnet.base.org"),
            "https://mainnet.base.org"
        );
    }
}
