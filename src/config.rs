use alloy::primitives::{Address, U256};
use eyre::{eyre, Result};
use std::env;
use std::str::FromStr;

/// Bot configuration loaded from environment variables / .env file.
pub struct Config {
    pub rpc_url: String,
    pub private_key: String,
    pub morpho: Address,
    pub arb_contract: Address,
    /// Token being flash-borrowed and arbitraged across two DEXes.
    pub loan_token: Address,
    /// Intermediate token used for the cross-DEX swap legs.
    pub quote_token: Address,
    /// First Uniswap-V2-style pair contract containing (loan_token, quote_token).
    pub pair_a: Address,
    /// Second Uniswap-V2-style pair contract containing (loan_token, quote_token).
    pub pair_b: Address,
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
        let pair_a = parse_addr("PAIR_A")?;
        let pair_b = parse_addr("PAIR_B")?;

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
            pair_a,
            pair_b,
            loan_amounts,
            min_profit,
            poll_interval_ms,
            dry_run,
        })
    }
}
