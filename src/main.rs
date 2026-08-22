use clap::{Parser, Subcommand};
use eyre::Result;
use morpho_arbitrage_bot::arbitrage::find_opportunity;
use morpho_arbitrage_bot::config::Config;
use morpho_arbitrage_bot::dex::fetch_reserves;
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
        Command::Scan => loop {
            if let Err(e) = run_once(&cfg).await {
                warn!(error = %e, "scan iteration failed");
            }
            tokio::time::sleep(std::time::Duration::from_millis(cfg.poll_interval_ms)).await;
        },
    }
    Ok(())
}

async fn run_once(cfg: &Config) -> Result<()> {
    let provider = alloy::providers::ProviderBuilder::new()
        .connect_http(cfg.rpc_url.parse()?);

    // Reserves are oriented with the loan token first; find_opportunity
    // flips them for the return leg as needed per venue pair.
    let mut pools = Vec::with_capacity(cfg.venues.len());
    for (idx, venue) in cfg.venues.iter().enumerate() {
        let reserves = fetch_reserves(&provider, venue.pair, cfg.loan_token).await?;
        pools.push(morpho_arbitrage_bot::arbitrage::PoolState {
            venue: idx,
            reserves,
            fee_bps: venue.fee_bps,
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

    info!(
        first = opp.first,
        second = opp.second,
        loan = %opp.loan_amount,
        profit = %opp.profit,
        "opportunity found"
    );

    let params = executor::build_params(cfg, &opp);

    executor::simulate(&provider, cfg.arb_contract, params.clone()).await?;

    if cfg.dry_run {
        info!("dry-run enabled; skipping broadcast");
        return Ok(());
    }

    let tx = executor::execute(&cfg.rpc_url, &cfg.private_key, cfg.arb_contract, params).await?;
    info!(tx = %tx, "arbitrage transaction confirmed");
    Ok(())
}
