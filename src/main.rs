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
    // flips them for the return leg as needed per direction.
    let reserves_a = fetch_reserves(&provider, cfg.pair_a, cfg.loan_token).await?;
    let reserves_b = fetch_reserves(&provider, cfg.pair_b, cfg.loan_token).await?;

    let best = cfg
        .loan_amounts
        .iter()
        .filter_map(|&size| find_opportunity(size, reserves_a, reserves_b, cfg.min_profit))
        .max_by_key(|o| o.profit);

    let Some(opp) = best else {
        info!("no profitable opportunity");
        return Ok(());
    };

    info!(
        direction = ?opp.direction,
        loan = %opp.loan_amount,
        profit = %opp.profit,
        "opportunity found"
    );

    // Routers must point at the venues' swap routers accepted by the contract;
    // pair addresses are used as placeholders until configured by the operator.
    let params = executor::build_params(cfg, &opp, cfg.pair_a, cfg.pair_b);

    executor::simulate(&provider, cfg.arb_contract, params.clone()).await?;

    if cfg.dry_run {
        info!("dry-run enabled; skipping broadcast");
        return Ok(());
    }

    let tx = executor::execute(&cfg.rpc_url, &cfg.private_key, cfg.arb_contract, params).await?;
    info!(tx = %tx, "arbitrage transaction confirmed");
    Ok(())
}
