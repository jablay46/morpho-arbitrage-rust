use alloy::primitives::{Address, U256};
use clap::{Parser, Subcommand};
use eyre::Result;
use morpho_arbitrage_bot::arbitrage::{find_opportunity, PoolState, V3PoolState};
use morpho_arbitrage_bot::config::{Config, VenueKind};
use morpho_arbitrage_bot::dex::{
    fetch_pair_tokens, fetch_scan_snapshot, fetch_v3_pair_tokens, orient_reserves, PairTokens,
    PoolReserves,
};
use morpho_arbitrage_bot::executor;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "morpho-arbitrage-bot", about = "Morpho flashloan arbitrage bot")]
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
/// traffic is only getReserves / slot0+liquidity / gasPrice (token0/token1
/// and the contract owner never change).
struct VenueCache {
    /// (token0, token1) per venue, aligned with cfg.venues.
    pair_tokens: Vec<PairTokens>,
    /// V2/Aero venue indices and their pair addresses, for the snapshot batch.
    v2_idx: Vec<usize>,
    v2_pairs: Vec<Address>,
    /// V3 venue indices and their pool addresses.
    v3_idx: Vec<usize>,
    v3_pools: Vec<Address>,
    /// Contract owner, used as `from` in simulations/gas estimates.
    owner: Address,
}

impl VenueCache {
    async fn build<P: alloy::providers::Provider>(provider: &P, cfg: &Config) -> Result<Self> {
        let mut pair_tokens = Vec::with_capacity(cfg.venues.len());
        let mut v2_idx = Vec::new();
        let mut v2_pairs = Vec::new();
        let mut v3_idx = Vec::new();
        let mut v3_pools = Vec::new();
        for (idx, venue) in cfg.venues.iter().enumerate() {
            // Auto-resolve the pool from the venue's factory when the
            // config says "auto" (pair = Address::ZERO).
            let pool = if venue.pair == Address::ZERO {
                let pool = morpho_arbitrage_bot::dex::resolve_pool(
                    provider,
                    venue.kind,
                    venue.factory,
                    venue.router,
                    cfg.loan_token,
                    cfg.quote_token,
                    venue.stable,
                    venue.fee_tier,
                )
                .await?;
                info!(venue = idx, pool = %pool, kind = ?venue.kind, "pool auto-resolved");
                pool
            } else {
                venue.pair
            };
            let tokens = if venue.kind == VenueKind::UniswapV3 {
                v3_idx.push(idx);
                v3_pools.push(pool);
                fetch_v3_pair_tokens(provider, pool).await?
            } else {
                v2_idx.push(idx);
                v2_pairs.push(pool);
                fetch_pair_tokens(provider, pool).await?
            };
            // Fail fast on misconfigured venues: the loan token must be in
            // the pair, otherwise orientation would error on every scan.
            if cfg.loan_token != tokens.token0 && cfg.loan_token != tokens.token1 {
                eyre::bail!(
                    "venue {idx} pool {pool} does not contain loan token {}",
                    cfg.loan_token
                );
            }
            pair_tokens.push(tokens);
        }
        let owner = executor::fetch_owner(provider, cfg.arb_contract).await?;
        Ok(Self {
            pair_tokens,
            v2_idx,
            v2_pairs,
            v3_idx,
            v3_pools,
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

    match cli.command {
        Command::Once => run_once(&cfg, &cache, &broadcaster).await?,
        Command::Scan => {
            if let Some(wss_url) = &cfg.wss_url {
                info!(wss = %wss_url, "starting event-driven scanning via WebSocket");
                run_event_driven(&cfg, &cache, wss_url, &broadcaster).await?;
            } else {
                info!(poll_ms = cfg.poll_interval_ms, "starting polling-based scanning");
                loop {
                    if let Err(e) = run_once(&cfg, &cache, &broadcaster).await {
                        warn!(error = %e, "scan iteration failed");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms)).await;
                }
            }
        }
    }
    Ok(())
}

/// Build the wallet-enabled HTTP provider used to broadcast trades.
fn build_broadcaster(cfg: &Config) -> Result<impl alloy::providers::Provider> {
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
async fn run_event_driven<B: alloy::providers::Provider>(
    cfg: &Config,
    cache: &VenueCache,
    wss_url: &str,
    broadcaster: &B,
) -> Result<()> {
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

/// Run one scan iteration with a given provider. All chain reads are served
/// by ONE JSON-RPC batch (reserves + V3 state + gas price).
async fn run_once_with_provider<P, B>(
    cfg: &Config,
    cache: &VenueCache,
    provider: &P,
    broadcaster: &B,
) -> Result<()>
where
    P: alloy::providers::Provider,
    B: alloy::providers::Provider,
{
    let snapshot = fetch_scan_snapshot(provider, &cache.v2_pairs, &cache.v3_pools).await?;
    let gas_price = cfg.gas_price_wei.unwrap_or(snapshot.gas_price);

    let mut pools: Vec<PoolState> = Vec::with_capacity(cfg.venues.len());
    for (j, &idx) in cache.v2_idx.iter().enumerate() {
        let (r0, r1) = snapshot.v2_raw[j];
        let venue = &cfg.venues[idx];
        match orient_reserves(r0, r1, &cache.pair_tokens[idx], venue.pair, cfg.loan_token) {
            Ok(reserves) => pools.push(PoolState {
                venue: idx,
                reserves,
                v3_state: None,
                fee_bps: venue.fee_bps,
                fee_tier: venue.fee_tier,
            }),
            Err(e) => warn!(venue = idx, error = %e, "skipping venue"),
        }
    }
    for (j, &idx) in cache.v3_idx.iter().enumerate() {
        let (sqrt_price_x96, liquidity) = snapshot.v3_raw[j];
        let venue = &cfg.venues[idx];
        pools.push(PoolState {
            venue: idx,
            reserves: PoolReserves {
                reserve_in: U256::ZERO,
                reserve_out: U256::ZERO,
            },
            v3_state: Some(V3PoolState {
                sqrt_price_x96,
                liquidity,
            }),
            fee_bps: venue.fee_bps,
            fee_tier: venue.fee_tier,
        });
    }

    let best = cfg
        .loan_amounts
        .iter()
        .filter_map(|&size| find_opportunity(size, &pools, cfg.min_profit))
        .max_by_key(|o| o.profit);

    let Some(opp) = best else {
        info!("no profitable opportunity");
        return Ok(());
    };

    // Estimate gas cost and subtract from profit. Gas is paid in ETH
    // (wrapped_native on L2); when the loan token differs, converting wei
    // into loan-token units would require a native/quote pool we don't
    // track, so conservatively compare gross profit instead of mixing
    // units (a false-negative is safer than a false-positive).
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
            warn!(
                loan_token = %cfg.loan_token,
                wrapped_native = %cfg.wrapped_native,
                "loan token is not wrapped native; gas cost conversion unavailable, comparing gross profit"
            );
            U256::ZERO
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

    executor::simulate(provider, cfg.arb_contract, cache.owner, params.clone()).await?;

    if cfg.dry_run {
        info!("dry-run enabled; skipping broadcast");
        return Ok(());
    }

    let tx = executor::execute(broadcaster, cfg.arb_contract, params).await?;
    info!(tx = %tx, "arbitrage transaction confirmed");
    Ok(())
}

/// Run one scan iteration (convenience wrapper for polling mode).
async fn run_once<B: alloy::providers::Provider>(
    cfg: &Config,
    cache: &VenueCache,
    broadcaster: &B,
) -> Result<()> {
    let provider = alloy::providers::ProviderBuilder::new()
        .connect_http(cfg.rpc_url.parse()?);
    run_once_with_provider(cfg, cache, &provider, broadcaster).await
}
