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
    /// Contract owner, used as `from` in simulations/gas estimates.
    owner: Address,
}

impl VenueCache {
    async fn build<P: alloy::providers::Provider>(provider: &P, cfg: &Config) -> Result<Self> {
        let mut pair_tokens = Vec::with_capacity(cfg.venues.len());
        let mut v2_idx = Vec::new();
        let mut v2_pairs = Vec::new();
        let mut v3_idx = Vec::new();
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
        }
        let owner = executor::fetch_owner(provider, cfg.arb_contract).await?;
        Ok(Self {
            pair_tokens,
            v2_idx,
            v2_pairs,
            v3_idx,
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
        dry_run = cfg.dry_run,
        "bot configured"
    );

    if cfg.loan_token != cfg.wrapped_native && cfg.gas_cost_loan.is_zero() && !cfg.dry_run {
        warn!(
            loan_token = %cfg.loan_token,
            wrapped_native = %cfg.wrapped_native,
            "loan token is not wrapped native and GAS_COST_LOAN is unset; \
             net-profit filtering degrades to gross-profit (gas unaccounted)"
        );
    }

    match cli.command {
        Command::Once => run_once(&cfg, &cache, &broadcaster).await?,
        Command::Scan => {
            if let Some(wss_url) = &cfg.wss_url {
                info!(wss = %wss_url, "starting event-driven scanning via WebSocket");
                run_event_driven(&cfg, &cache, wss_url, &broadcaster).await?;
            } else {
                info!(
                    poll_ms = cfg.poll_interval_ms,
                    "starting polling-based scanning"
                );
                loop {
                    if let Err(e) = run_once(&cfg, &cache, &broadcaster).await {
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

/// Event-driven scanning: subscribe to newHeads via WebSocket, scan per block.
/// On subscription failure or stream end, falls back to polling so the bot
/// keeps running (a dropped WSS connection must not kill the process).
async fn run_event_driven<B>(
    cfg: &Config,
    cache: &VenueCache,
    wss_url: &str,
    broadcaster: &B,
) -> Result<()>
where
    B: alloy::providers::Provider + Clone + 'static,
{
    use alloy::providers::Provider;
    use futures::StreamExt;

    let ws = alloy::rpc::client::WsConnect::new(wss_url);
    let client = alloy::rpc::client::RpcClient::connect_pubsub(ws).await?;
    let provider = alloy::providers::RootProvider::<alloy::network::Ethereum>::new(client);

    let sub = provider.subscribe_blocks().await?;
    info!("subscribed to newHeads; scanning per block");
    let mut stream = sub.into_stream();
    while let Some(header) = stream.next().await {
        info!(block = header.number, "new block; scanning");
        if let Err(e) = run_once_with_provider(cfg, cache, &provider, broadcaster).await {
            warn!(error = %e, "event-driven scan failed");
        }
    }
    warn!("block subscription ended; falling back to polling");
    loop {
        if let Err(e) = run_once(cfg, cache, broadcaster).await {
            warn!(error = %e, "scan iteration failed");
        }
        tokio::time::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms)).await;
    }
}

/// Run one scan iteration with a given provider. Chain reads happen in two
/// JSON-RPC batches: phase 1 fetches V2 reserves, V3 leg-1 quotes (one
/// QuoterV2 call per venue x loan size) and the gas price; phase 2 quotes
/// V3 leg 2, whose inputs are only known once leg 1 has been priced.
async fn run_once_with_provider<P, B>(
    cfg: &Config,
    cache: &VenueCache,
    provider: &P,
    broadcaster: &B,
) -> Result<()>
where
    P: alloy::providers::Provider,
    B: alloy::providers::Provider + Clone + 'static,
{
    let sizes = &cfg.loan_amounts;

    // Phase 1 batch: reserves + leg-1 quotes (loan -> quote) + gas price.
    let mut leg1_requests = Vec::with_capacity(cache.v3_idx.len() * sizes.len());
    for &idx in &cache.v3_idx {
        for &size in sizes {
            leg1_requests.push(QuoteRequest {
                token_in: cfg.loan_token,
                token_out: cfg.quote_token,
                fee_tier: cfg.venues[idx].fee_tier,
                amount_in: size,
            });
        }
    }
    let snapshot =
        fetch_scan_snapshot(provider, cfg.quoter_v2, &cache.v2_pairs, &leg1_requests).await?;
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
                    },
                ));
            }
        }
    }
    if !phase2.is_empty() {
        let requests: Vec<QuoteRequest> = phase2.iter().map(|(_, r)| *r).collect();
        let results = fetch_quotes(provider, cfg.quoter_v2, &requests).await?;
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

    // Estimate gas cost and subtract from profit. Gas is paid in ETH
    // (wrapped_native on L2); when the loan token differs there is no
    // on-the-fly conversion, so the configured GAS_COST_LOAN fallback is
    // used instead (warned about at startup when unset).
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
        let gas_cost_wei = gas_estimate * gas_price;
        if cfg.loan_token == cfg.wrapped_native {
            gas_cost_wei
        } else {
            cfg.gas_cost_loan
        }
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

    // eth_estimateGas fully executes the tx, so this call doubles as the
    // pre-broadcast simulation gate on the FINAL params — a separate
    // eth_call would only repeat the same execution and add latency.
    executor::estimate_gas(provider, cfg.arb_contract, cache.owner, params.clone()).await?;

    // Fire-and-forget: returns once the node accepts the tx; the receipt is
    // awaited on a background task so the scan loop is never blocked.
    let tx = executor::execute(broadcaster.clone(), cfg.arb_contract, params).await?;
    info!(tx = %tx, "arbitrage transaction broadcast");
    Ok(())
}

/// Run one scan iteration (convenience wrapper for polling mode).
async fn run_once<B>(cfg: &Config, cache: &VenueCache, broadcaster: &B) -> Result<()>
where
    B: alloy::providers::Provider + Clone + 'static,
{
    let provider = alloy::providers::ProviderBuilder::new().connect_http(cfg.rpc_url.parse()?);
    run_once_with_provider(cfg, cache, &provider, broadcaster).await
}
