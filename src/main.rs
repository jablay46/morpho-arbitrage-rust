use alloy::primitives::{Address, U256};
use clap::{Parser, Subcommand};
use eyre::Result;
use morpho_arbitrage_bot::arbitrage::{find_opportunity, v2_quotes, VenueQuotes};
use morpho_arbitrage_bot::config::{Config, VenueKind};
use morpho_arbitrage_bot::dex::{
    fetch_pair_tokens, fetch_quotes, fetch_scan_snapshot, fetch_v3_pair_tokens, orient_reserves,
    PairTokens, QuoteRequest,
};
use morpho_arbitrage_bot::executor;
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

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
    /// V3 venue indices (priced via QuoterV2, no per-scan pool reads).
    v3_idx: Vec<usize>,
    /// All resolved pool addresses (V2 + V3), for the event filter.
    pool_addrs: Vec<Address>,
    /// Contract owner, used as `from` in simulations/gas estimates.
    owner: Address,
}

impl VenueCache {
    async fn build<P: alloy::providers::Provider>(provider: &P, cfg: &Config) -> Result<Self> {
        let mut pair_tokens = Vec::with_capacity(cfg.venues.len());
        let mut v2_idx = Vec::new();
        let mut v2_pairs = Vec::new();
        let mut v3_idx = Vec::new();
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
                fetch_v3_pair_tokens(provider, pool).await?
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
        Ok(Self {
            pair_tokens,
            v2_idx,
            v2_pairs,
            v3_idx,
            pool_addrs,
            owner,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    // Broadcast provider with the signing wallet attached, built once and
    // reused across scans (executor no longer opens a fresh connection per
    // trade).
    let broadcaster = build_broadcaster(&cfg)?;

    // One-off startup resolution: pair tokens for orientation/validation and
    // the contract owner for simulations. ~3 RPC calls per venue, once.
    let cache = VenueCache::build(&broadcaster, &cfg).await?;

    info!(
        morpho = %cfg.morpho,
        arb_contract = %cfg.arb_contract,
        owner = %cache.owner,
        loan_token = %cfg.loan_token,
        quote_token = %cfg.quote_token,
        venues = cfg.venues.len(),
        loan_amounts = ?cfg.loan_amounts,
        dry_run = cfg.dry_run,
        "bot configured"
    );
    for (i, v) in cfg.venues.iter().enumerate() {
        info!(
            venue = i,
            pair = %v.pair,
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
                run_event_driven(&cfg, &cache, wss_url, &broadcaster, &inflight).await?;
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
/// otherwise the global QUOTER_V2.
fn resolve_quoter(cfg: &Config, venue: &morpho_arbitrage_bot::config::Venue) -> Address {
    if venue.quoter == Address::ZERO {
        cfg.quoter_v2
    } else {
        venue.quoter
    }
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
/// pool: V2 Sync/Swap/Mint/Burn and V3 Swap/Mint/Burn.
fn pool_event_signatures() -> Vec<alloy::primitives::B256> {
    [
        "Sync(uint112,uint112)",
        "Swap(address,uint256,uint256,uint256,uint256,address)",
        "Mint(address,uint256,uint256)",
        "Burn(address,uint256,uint256,address)",
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
    cache: &VenueCache,
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
    let filter = alloy::rpc::types::Filter::new()
        .address(cache.pool_addrs.clone())
        .event_signature(pool_event_signatures());
    let mut sweep_every = cfg.sweep_interval_blocks;
    let mut logs = match provider.subscribe_logs(&filter).await {
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
                    Some(log) => match log.block_number {
                        // Multiple pool events within one block coalesce into
                        // a single scan: reads are pinned to the sealed
                        // latest block, so later events add nothing.
                        Some(block) if block > last_scanned => Some(("pool event", block)),
                        _ => None,
                    },
                }
            }
        };
        let Some((reason, block)) = trigger else {
            continue;
        };
        info!(block, reason, "scanning");
        match run_once_with_provider(cfg, cache, &provider, broadcaster, Some(inflight)).await {
            Ok(()) => last_scanned = last_scanned.max(block),
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

/// Run one scan iteration with a given provider. Chain reads happen in two
/// JSON-RPC batches, both pinned to the same block: phase 1 fetches V2
/// reserves, V3 leg-1 quotes (one QuoterV2 call per venue x loan size) and
/// the gas price; phase 2 quotes V3 leg 2, whose inputs are only known once
/// leg 1 has been priced. Pinning both phases to one block keeps the two
/// legs of a cycle priced against a consistent chain state.
async fn run_once_with_provider<P, B>(
    cfg: &Config,
    cache: &VenueCache,
    provider: &P,
    broadcaster: &B,
    inflight: Option<&InflightFlag>,
) -> Result<()>
where
    P: alloy::providers::Provider,
    B: alloy::providers::Provider + Clone + 'static,
{
    let sizes = &cfg.loan_amounts;

    // Pin both phases to one block so the legs of a cycle are priced
    // against the same chain state ("latest" could advance between the
    // two batches).
    let block = alloy::eips::BlockId::number(provider.get_block_number().await?);

    // Phase 1 batch: reserves + leg-1 quotes (loan -> quote) + gas price.
    let mut leg1_requests = Vec::with_capacity(cache.v3_idx.len() * sizes.len());
    for &idx in &cache.v3_idx {
        for &size in sizes {
            leg1_requests.push(QuoteRequest {
                token_in: cfg.loan_token,
                token_out: cfg.quote_token,
                fee_tier: cfg.venues[idx].fee_tier,
                amount_in: size,
                quoter: resolve_quoter(cfg, &cfg.venues[idx]),
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
    for (j, &idx) in cache.v3_idx.iter().enumerate() {
        let leg1: Vec<Option<U256>> = snapshot.v3_quotes[j * n_sizes..(j + 1) * n_sizes].to_vec();
        if leg1.iter().all(|q| q.is_none()) {
            warn!(
                venue = idx,
                "V3 venue returned no usable quotes; skipping venue"
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
        if let Some(reserves) = legs[s].v2_reserves {
            legs[s].quotes.leg2 = v2_quotes(
                venue_idx,
                reserves,
                cfg.venues[venue_idx].fee_bps,
                &[],
                &inputs,
            )
            .leg2;
        } else {
            for q in inputs {
                phase2.push((
                    s,
                    QuoteRequest {
                        token_in: cfg.quote_token,
                        token_out: cfg.loan_token,
                        fee_tier: cfg.venues[venue_idx].fee_tier,
                        amount_in: q,
                        quoter: resolve_quoter(cfg, &cfg.venues[venue_idx]),
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
    let Some(opp) = find_opportunity(sizes, &quotes, cfg.min_profit) else {
        info!("no profitable opportunity");
        return Ok(());
    };

    // Estimate gas cost and subtract from profit. Gas is paid in ETH;
    // config enforces loan_token == wrapped_native, so the wei estimate
    // is directly comparable to profit in loan-token units.
    //
    // Two-stage build: estimate gas with a provisional params (minProfit
    // barely affects calldata size/gas), then rebuild with the on-chain
    // backstop raised to min_profit + gas so the contract itself reverts
    // net-unprofitable trades before they waste gas on-chain. Skipped
    // entirely in dry-run mode (no trade will be sent anyway).
    let gas_cost_loan = if cfg.dry_run {
        U256::ZERO
    } else {
        let provisional = executor::build_params(cfg, &opp, cfg.min_profit);
        let gas_estimate =
            executor::estimate_gas(provider, cfg.arb_contract, cache.owner, provisional).await?;
        gas_estimate * gas_price
    };

    let net_profit = opp.profit.saturating_sub(gas_cost_loan);
    let onchain_min_profit = cfg.min_profit + gas_cost_loan;
    let params = executor::build_params(cfg, &opp, onchain_min_profit);
    if net_profit < cfg.min_profit {
        info!(
            gross = %opp.profit,
            gas = %gas_cost_loan,
            net = %net_profit,
            "opportunity filtered out by gas cost"
        );
        return Ok(());
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
        executor::simulate(provider, cfg.arb_contract, cache.owner, params).await?;
        info!("dry-run enabled; skipping broadcast");
        return Ok(());
    }

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
            return Ok(());
        }
    }
    // Fire-and-forget: returns once the node accepts the tx; the receipt
    // watcher clears the in-flight flag on inclusion.
    match executor::execute(
        broadcaster.clone(),
        cfg.arb_contract,
        params,
        inflight.cloned(),
    )
    .await
    {
        Ok(tx) => info!(tx = %tx, "arbitrage transaction broadcast"),
        Err(e) => {
            if let Some(flag) = inflight {
                flag.store(false, std::sync::atomic::Ordering::Release);
            }
            return Err(e);
        }
    }
    Ok(())
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
    run_once_with_provider(cfg, cache, &provider, broadcaster, inflight).await
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
        assert_eq!(sigs.len(), 7);
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
