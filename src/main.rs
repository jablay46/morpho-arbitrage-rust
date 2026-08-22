use alloy::primitives::{Address, U256};
use clap::{Parser, Subcommand};
use eyre::Result;
use morpho_arbitrage_bot::arbitrage::{find_opportunity, PoolState};
use morpho_arbitrage_bot::config::{Config, VenueKind};
use morpho_arbitrage_bot::dex::{fetch_reserves, fetch_reserves_batched, fetch_v3_pool_state};
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cli = Cli::parse();
    let cfg = Config::from_env()?;
    info!(
        morpho = %cfg.morpho,
        arb_contract = %cfg.arb_contract,
        loan_token = %cfg.loan_token,
        quote_token = %cfg.quote_token,
        venues = cfg.venues.len(),
        dry_run = cfg.dry_run,
        "bot configured"
    );

    match cli.command {
        Command::Once => run_once(&cfg).await?,
        Command::Scan => {
            if let Some(wss_url) = &cfg.wss_url {
                info!(wss = %wss_url, "starting event-driven scanning via WebSocket");
                run_event_driven(&cfg, wss_url).await?;
            } else {
                info!(poll_ms = cfg.poll_interval_ms, "starting polling-based scanning");
                loop {
                    if let Err(e) = run_once(&cfg).await {
                        warn!(error = %e, "scan iteration failed");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms)).await;
                }
            }
        }
    }
    Ok(())
}

/// Event-driven scanning: subscribe to newHeads via WebSocket, scan per block.
/// On subscription failure or stream end, falls back to polling so the bot
/// keeps running (a dropped WSS connection must not kill the process).
async fn run_event_driven(cfg: &Config, wss_url: &str) -> Result<()> {
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
        if let Err(e) = run_once_with_provider(cfg, &provider).await {
            warn!(error = %e, "event-driven scan failed");
        }
    }
    warn!("block subscription ended; falling back to polling");
    loop {
        if let Err(e) = run_once(cfg).await {
            warn!(error = %e, "scan iteration failed");
        }
        tokio::time::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms)).await;
    }
}

/// Fetch pool state for all venues. V2/Aero reserves are fetched in one
/// JSON-RPC batch when possible; V3 venues are excluded (V3 pools have no
/// getReserves()) and get their state via fetch_v3_pool_state instead.
async fn fetch_all_reserves<P: alloy::providers::Provider>(
    provider: &P,
    cfg: &Config,
) -> Result<Vec<PoolState>> {
    use morpho_arbitrage_bot::dex::PoolReserves;

    // V2/Aero venues get reserves; V3 venues get slot0+liquidity.
    let v2_idx: Vec<usize> = cfg
        .venues
        .iter()
        .enumerate()
        .filter(|(_, v)| v.kind != VenueKind::UniswapV3)
        .map(|(i, _)| i)
        .collect();
    let v2_pairs: Vec<(Address, Address)> = v2_idx
        .iter()
        .map(|&i| (cfg.venues[i].pair, cfg.loan_token))
        .collect();

    // Try batched fetch first; fall back to serial if it fails.
    let v2_reserves = match fetch_reserves_batched(provider, &v2_pairs).await {
        Ok(r) => {
            info!(count = r.len(), "reserves fetched via JSON-RPC batch");
            r
        }
        Err(e) => {
            warn!(error = %e, "batched fetch failed; falling back to serial");
            let mut results = Vec::with_capacity(v2_idx.len());
            for &i in &v2_idx {
                results.push(fetch_reserves(provider, cfg.venues[i].pair, cfg.loan_token).await?);
            }
            results
        }
    };
    let mut reserve_by_idx: Vec<Option<PoolReserves>> = vec![None; cfg.venues.len()];
    for (j, &i) in v2_idx.iter().enumerate() {
        reserve_by_idx[i] = Some(v2_reserves[j]);
    }

    let mut pools = Vec::with_capacity(cfg.venues.len());
    for (idx, venue) in cfg.venues.iter().enumerate() {
        let (reserves, v3_state) = if venue.kind == VenueKind::UniswapV3 {
            let (sqrt_price_x96, liquidity) =
                fetch_v3_pool_state(provider, venue.pair, cfg.loan_token).await?;
            (
                PoolReserves {
                    reserve_in: U256::ZERO,
                    reserve_out: U256::ZERO,
                },
                Some(morpho_arbitrage_bot::arbitrage::V3PoolState {
                    sqrt_price_x96,
                    liquidity,
                }),
            )
        } else {
            (
                reserve_by_idx[idx].expect("non-V3 venue fetched in batch"),
                None,
            )
        };
        pools.push(PoolState {
            venue: idx,
            reserves,
            v3_state,
            fee_bps: venue.fee_bps,
            fee_tier: venue.fee_tier,
        });
    }
    Ok(pools)
}

/// Run one scan iteration with a given provider.
async fn run_once_with_provider<P: alloy::providers::Provider>(
    cfg: &Config,
    provider: &P,
) -> Result<()> {
    let pools = fetch_all_reserves(provider, cfg).await?;

    // Fetch gas price for cost calculation.
    let gas_price = match cfg.gas_price_wei {
        Some(gp) => gp,
        None => provider.get_gas_price().await.map(U256::from).unwrap_or_default(),
    };

    let best = cfg
        .loan_amounts
        .iter()
        .filter_map(|&size| find_opportunity(size, &pools, cfg.min_profit))
        .max_by_key(|o| o.profit);

    let Some(opp) = best else {
        info!("no profitable opportunity");
        return Ok(());
    };

    // Estimate gas cost and subtract from profit, converting wei to
    // loan-token units. Gas is paid in ETH (wrapped_native on L2); when the
    // loan token differs, price 1 wei of native in loan-token units via the
    // venue pools. Spot price of native in loan-token units is
    // reserve_loan / reserve_native on a native/loan pool; we only have
    // loan/quote pools, so derive it through the quote leg of the winning
    // route: quote_out per loan token is the pool's loan->quote price, and
    // for a stable quote (USDC) that is also ~the USD price. Native->quote
    // needs a native/quote pool which we don't track, so when the loan
    // token is not wrapped native we conservatively skip the conversion
    // (compare gross profit) rather than mixing wei with loan units.
    let params = executor::build_params(cfg, &opp);
    let gas_estimate = executor::estimate_gas(provider, cfg.arb_contract, params.clone()).await?;
    let gas_cost_wei = gas_estimate * gas_price;
    let gas_cost_loan = if cfg.loan_token == cfg.wrapped_native {
        gas_cost_wei
    } else {
        warn!(
            loan_token = %cfg.loan_token,
            wrapped_native = %cfg.wrapped_native,
            "loan token is not wrapped native; gas cost conversion unavailable, comparing gross profit"
        );
        U256::ZERO
    };

    let net_profit = opp.profit.saturating_sub(gas_cost_loan);
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

    executor::simulate(provider, cfg.arb_contract, params.clone()).await?;

    if cfg.dry_run {
        info!("dry-run enabled; skipping broadcast");
        return Ok(());
    }

    let tx = executor::execute(&cfg.rpc_url, &cfg.private_key, cfg.arb_contract, params).await?;
    info!(tx = %tx, "arbitrage transaction confirmed");
    Ok(())
}

/// Run one scan iteration (convenience wrapper for polling mode).
async fn run_once(cfg: &Config) -> Result<()> {
    let provider = alloy::providers::ProviderBuilder::new()
        .connect_http(cfg.rpc_url.parse()?);
    run_once_with_provider(cfg, &provider).await
}
