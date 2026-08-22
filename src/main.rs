use alloy::primitives::U256;
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
/// Note: alloy's WebSocket support requires the `ws` feature which is not
/// available in alloy 1.x. This is a placeholder for when WebSocket support
/// is added via a custom transport or a different crate.
async fn run_event_driven(cfg: &Config, _wss_url: &str) -> Result<()> {
    // TODO: Implement WebSocket subscription when alloy supports it or
    // via a custom transport. For now, fall back to polling.
    warn!("WebSocket event-driven scanning not yet implemented; falling back to polling");
    loop {
        if let Err(e) = run_once(cfg).await {
            warn!(error = %e, "scan iteration failed");
        }
        tokio::time::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms)).await;
    }
}

/// Fetch reserves for all venues, using batched calls when possible.
async fn fetch_all_reserves<P: alloy::providers::Provider>(
    provider: &P,
    cfg: &Config,
) -> Result<Vec<PoolState>> {
    let mut venues = Vec::with_capacity(cfg.venues.len());
    for venue in &cfg.venues {
        venues.push((venue.pair, cfg.loan_token));
    }

    // Try batched fetch first; fall back to serial if it fails.
    let reserves = match fetch_reserves_batched(provider, &venues).await {
        Ok(r) => r,
        Err(_) => {
            // Fallback to serial for providers that don't support batching.
            let mut results = Vec::with_capacity(cfg.venues.len());
            for venue in &cfg.venues {
                results.push(fetch_reserves(provider, venue.pair, cfg.loan_token).await?);
            }
            results
        }
    };

    let mut pools = Vec::with_capacity(cfg.venues.len());
    for (idx, (venue, reserve)) in cfg.venues.iter().zip(reserves.iter()).enumerate() {
        let v3_state = if venue.kind == VenueKind::UniswapV3 {
            let (sqrt_price_x96, liquidity) = fetch_v3_pool_state(provider, venue.pair, cfg.loan_token).await?;
            Some(morpho_arbitrage_bot::arbitrage::V3PoolState {
                sqrt_price_x96,
                liquidity,
            })
        } else {
            None
        };
        pools.push(PoolState {
            venue: idx,
            reserves: *reserve,
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

    // Estimate gas cost and subtract from profit.
    let params = executor::build_params(cfg, &opp);
    let gas_estimate = executor::estimate_gas(provider, cfg.arb_contract, params.clone()).await?;
    let gas_cost_wei = gas_estimate * gas_price;
    let gas_cost_quote = if cfg.loan_token == cfg.quote_token {
        gas_cost_wei
    } else {
        // Convert wei to loan token units via the pool price.
        // For simplicity, assume 1:1 if not convertible; production should use oracle.
        gas_cost_wei
    };

    let net_profit = opp.profit.saturating_sub(gas_cost_quote);
    if net_profit < cfg.min_profit {
        info!(
            gross = %opp.profit,
            gas = %gas_cost_quote,
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
        gas = %gas_cost_quote,
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
