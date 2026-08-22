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

    // Broadcast provider with the signing wallet attached, built once and
    // reused across scans (executor no longer opens a fresh connection per
    // trade).
    let broadcaster = build_broadcaster(&cfg)?;

    match cli.command {
        Command::Once => run_once(&cfg, &broadcaster).await?,
        Command::Scan => {
            if let Some(wss_url) = &cfg.wss_url {
                info!(wss = %wss_url, "starting event-driven scanning via WebSocket");
                run_event_driven(&cfg, wss_url, &broadcaster).await?;
            } else {
                info!(poll_ms = cfg.poll_interval_ms, "starting polling-based scanning");
                loop {
                    if let Err(e) = run_once(&cfg, &broadcaster).await {
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
fn build_broadcaster(
    cfg: &Config,
) -> Result<impl alloy::providers::Provider> {
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
        if let Err(e) = run_once_with_provider(cfg, &provider, broadcaster).await {
            warn!(error = %e, "event-driven scan failed");
        }
    }
    warn!("block subscription ended; falling back to polling");
    loop {
        if let Err(e) = run_once(cfg, broadcaster).await {
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

    // Try batched fetch first; fall back to serial per-venue fetches that
    // skip a dead venue instead of aborting the whole scan (one bad pair
    // must not blind the bot to opportunities elsewhere).
    let mut reserve_by_idx: Vec<Option<PoolReserves>> = vec![None; cfg.venues.len()];
    match fetch_reserves_batched(provider, &v2_pairs).await {
        Ok(r) => {
            info!(count = r.len(), "reserves fetched via JSON-RPC batch");
            for (j, &i) in v2_idx.iter().enumerate() {
                reserve_by_idx[i] = Some(r[j]);
            }
        }
        Err(e) => {
            warn!(error = %e, "batched fetch failed; falling back to serial");
            for &i in &v2_idx {
                match fetch_reserves(provider, cfg.venues[i].pair, cfg.loan_token).await {
                    Ok(r) => reserve_by_idx[i] = Some(r),
                    Err(e) => {
                        warn!(venue = i, pair = %cfg.venues[i].pair, error = %e,
                            "venue reserve fetch failed; skipping venue");
                    }
                }
            }
        }
    }

    let mut pools = Vec::with_capacity(cfg.venues.len());
    for (idx, venue) in cfg.venues.iter().enumerate() {
        if venue.kind == VenueKind::UniswapV3 {
            match fetch_v3_pool_state(provider, venue.pair, cfg.loan_token).await {
                Ok((sqrt_price_x96, liquidity)) => pools.push(PoolState {
                    venue: idx,
                    reserves: PoolReserves {
                        reserve_in: U256::ZERO,
                        reserve_out: U256::ZERO,
                    },
                    v3_state: Some(morpho_arbitrage_bot::arbitrage::V3PoolState {
                        sqrt_price_x96,
                        liquidity,
                    }),
                    fee_bps: venue.fee_bps,
                    fee_tier: venue.fee_tier,
                }),
                Err(e) => {
                    warn!(venue = idx, pair = %venue.pair, error = %e,
                        "V3 pool state fetch failed; skipping venue");
                }
            }
            continue;
        }
        let Some(reserves) = reserve_by_idx[idx] else {
            // Venue failed in both batch and serial paths; skip it.
            continue;
        };
        pools.push(PoolState {
            venue: idx,
            reserves,
            v3_state: None,
            fee_bps: venue.fee_bps,
            fee_tier: venue.fee_tier,
        });
    }
    Ok(pools)
}

/// Run one scan iteration with a given provider.
async fn run_once_with_provider<P, B>(
    cfg: &Config,
    provider: &P,
    broadcaster: &B,
) -> Result<()>
where
    P: alloy::providers::Provider,
    B: alloy::providers::Provider,
{
    let pools = fetch_all_reserves(provider, cfg).await?;

    // Fetch gas price for cost calculation. A missing/again-zero gas price
    // must not silently zero out the gas term (that would re-admit
    // false-positive trades), so propagate RPC failures instead.
    let gas_price = match cfg.gas_price_wei {
        Some(gp) => gp,
        None => U256::from(provider.get_gas_price().await?),
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

    // Estimate gas cost and subtract from profit. Gas is paid in ETH
    // (wrapped_native on L2); when the loan token differs, converting wei
    // into loan-token units would require a native/quote pool we don't
    // track, so conservatively compare gross profit instead of mixing
    // units (a false-negative is safer than a false-positive).
    //
    // Two-stage build: estimate gas with a provisional params (minProfit
    // barely affects calldata size/gas), then rebuild with the on-chain
    // backstop raised to min_profit + gas so the contract itself reverts
    // net-unprofitable trades before they waste gas on-chain.
    let provisional = executor::build_params(cfg, &opp, cfg.min_profit);
    let gas_estimate =
        executor::estimate_gas(provider, cfg.arb_contract, provisional).await?;
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

    executor::simulate(provider, cfg.arb_contract, params.clone()).await?;

    if cfg.dry_run {
        info!("dry-run enabled; skipping broadcast");
        return Ok(());
    }

    let tx = executor::execute(broadcaster, cfg.arb_contract, params).await?;
    info!(tx = %tx, "arbitrage transaction confirmed");
    Ok(())
}

/// Run one scan iteration (convenience wrapper for polling mode).
async fn run_once<B: alloy::providers::Provider>(cfg: &Config, broadcaster: &B) -> Result<()> {
    let provider = alloy::providers::ProviderBuilder::new()
        .connect_http(cfg.rpc_url.parse()?);
    run_once_with_provider(cfg, &provider, broadcaster).await
}
