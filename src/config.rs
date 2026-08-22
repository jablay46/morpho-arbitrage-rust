use alloy::primitives::{Address, U256};
use eyre::{eyre, Result};
use std::env;
use std::str::FromStr;

/// One tradable venue: a Uniswap-V2-style pool plus its swap router.
pub struct Venue {
    pub pair: Address,
    pub router: Address,
}

/// Bot configuration loaded from environment variables / .env file.
pub struct Config {
    pub rpc_url: String,
    pub private_key: String,
    pub morpho: Address,
    pub arb_contract: Address,
    /// Token being flash-borrowed and arbitraged across DEXes.
    pub loan_token: Address,
    /// Intermediate token used for the cross-DEX swap legs.
    pub quote_token: Address,
    /// All DEX venues arbitraged against each other (at least two).
    pub venues: Vec<Venue>,
    /// Flash loan sizes to probe, in loan_token base units.
    pub loan_amounts: Vec<U256>,
    /// Minimum net profit (in loan_token base units) required to execute.
    pub min_profit: U256,
    /// Poll interval between scans, milliseconds.
    pub poll_interval_ms: u64,
    /// If true, never broadcast transactions; only log simulated results.
    pub dry_run: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let parse_addr = |key: &str| -> Result<Address> {
            let raw = env::var(key).map_err(|_| eyre!("missing env var {key}"))?;
            Address::from_str(&raw).map_err(|e| eyre!("invalid address in {key}: {e}"))
        };

        let rpc_url = env::var("RPC_URL").map_err(|_| eyre!("missing env var RPC_URL"))?;
        let private_key =
            env::var("PRIVATE_KEY").map_err(|_| eyre!("missing env var PRIVATE_KEY"))?;

        let morpho = parse_addr("MORPHO_ADDRESS")?;
        let arb_contract = parse_addr("ARB_CONTRACT")?;
        let loan_token = parse_addr("LOAN_TOKEN")?;
        let quote_token = parse_addr("QUOTE_TOKEN")?;

        // DEX venues as comma-separated `pair:router` entries, e.g.
        // DEX_VENUES=0xPair1:0xRouter1,0xPair2:0xRouter2,...
        let venues_raw =
            env::var("DEX_VENUES").map_err(|_| eyre!("missing env var DEX_VENUES"))?;
        let venues = venues_raw
            .split(',')
            .map(|entry| {
                let entry = entry.trim();
                let (pair, router) = entry.split_once(':').ok_or_else(|| {
                    eyre!("invalid DEX_VENUES entry '{entry}', expected <pair>:<router>")
                })?;
                Ok::<_, eyre::Report>(Venue {
                    pair: Address::from_str(pair.trim())
                        .map_err(|e| eyre!("invalid pair in DEX_VENUES '{entry}': {e}"))?,
                    router: Address::from_str(router.trim())
                        .map_err(|e| eyre!("invalid router in DEX_VENUES '{entry}': {e}"))?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if venues.len() < 2 {
            return Err(eyre!("DEX_VENUES needs at least two venues"));
        }

        let loan_amounts = env::var("LOAN_AMOUNTS")
            .unwrap_or_else(|_| "1000000000000000000".to_string())
            .split(',')
            .map(|s| {
                U256::from_str(s.trim())
                    .map_err(|e| eyre!("invalid LOAN_AMOUNTS entry '{s}': {e}"))
            })
            .collect::<Result<Vec<_>>>()?;

        let min_profit = env::var("MIN_PROFIT")
            .ok()
            .map(|s| U256::from_str(&s))
            .transpose()
            .map_err(|e| eyre!("invalid MIN_PROFIT: {e}"))?
            .unwrap_or(U256::ZERO);

        let poll_interval_ms = env::var("POLL_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5_000);

        let dry_run = env::var("DRY_RUN")
            .map(|s| matches!(s.as_str(), "1" | "true" | "yes"))
            .unwrap_or(true);

        Ok(Self {
            rpc_url,
            private_key,
            morpho,
            arb_contract,
            loan_token,
            quote_token,
            venues,
            loan_amounts,
            min_profit,
            poll_interval_ms,
            dry_run,
        })
    }
}
