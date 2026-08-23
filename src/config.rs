use alloy::primitives::{Address, U256};
use eyre::{eyre, Result};
use std::env;
use std::str::FromStr;

/// Router family of a venue; must match `KIND_*` constants in the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VenueKind {
    /// Uniswap-V2-style router taking `address[] path` (also Sushiswap V2,
    /// Pancakeswap V2).
    UniswapV2 = 0,
    /// Aerodrome-style router taking `Route[]` structs.
    Aerodrome = 1,
    /// Uniswap-V3-style router using `exactInputSingle` with `sqrtPriceLimitX96`.
    UniswapV3 = 2,
    /// Uniswap-V4-style PoolManager using `unlock` + `swap` with hooks.
    UniswapV4 = 3,
}

/// One tradable venue: a pool plus its swap router and fee model.
pub struct Venue {
    /// Pool/pair address, or Address::ZERO to auto-resolve from the factory
    /// at startup (requires `factory`).
    pub pair: Address,
    pub router: Address,
    pub kind: VenueKind,
    /// Pool fee in basis points charged on the input amount (30 = 0.3%).
    pub fee_bps: u64,
    /// Pool factory. Required when `pair` is zero (auto-resolve);
    /// Address::ZERO for Aerodrome means the router's default factory.
    pub factory: Address,
    /// Aerodrome stable-pool flag. Unused for V2/V3/V4.
    pub stable: bool,
    /// Uniswap V3 fee tier in hundredths of a bip (500 = 0.05%). Unused for V2/Aero.
    pub fee_tier: u32,
    /// Uniswap V4 pool ID (bytes32) for PoolManager. Unused for V2/V3.
    pub pool_id: [u8; 32],
    /// Per-venue QuoterV2 override (V3 only). Address::ZERO = use the
    /// global `Config::quoter_v2`. Needed for V3 venues whose quotes live
    /// on a different deployment (e.g. PancakeSwap V3), since each factory
    /// has its own quoter contract.
    pub quoter: Address,
}

impl Venue {
    /// For V2/Aero this is the pair address; for V3 it's the pool address;
    /// for V4 it's unused (PoolManager handles routing via pool_id).
    pub fn pool_address(&self) -> Address {
        self.pair
    }
}

/// Bot configuration loaded from environment variables / .env file.
pub struct Config {
    pub rpc_url: String,
    /// WebSocket URL for event-driven scanning (Chainstack, Alchemy, etc.).
    /// If None, falls back to polling.
    pub wss_url: Option<String>,
    pub private_key: String,
    pub morpho: Address,
    pub arb_contract: Address,
    /// Token being flash-borrowed and arbitraged across DEXes.
    pub loan_token: Address,
    /// Intermediate token used for the cross-DEX swap legs.
    pub quote_token: Address,
    /// Wrapped native token (e.g. WETH on Base); used to convert gas cost
    /// (paid in ETH) into loan-token units.
    pub wrapped_native: Address,
    /// All DEX venues arbitraged against each other (at least two).
    pub venues: Vec<Venue>,
    /// Flash loan sizes to probe, in loan_token base units.
    pub loan_amounts: Vec<U256>,
    /// Minimum net profit (in loan_token base units) required to execute.
    pub min_profit: U256,
    /// Gas price in wei for cost calculation. If None, fetched on-chain.
    pub gas_price_wei: Option<U256>,
    /// Slippage tolerance per swap leg, in basis points (50 = 0.5%). The
    /// simulated leg output scaled by (1 - slippage) becomes the on-chain
    /// `minOut`, bounding price drift and raising the cost of sandwiching.
    pub slippage_bps: u64,
    /// Poll interval between scans, milliseconds.
    pub poll_interval_ms: u64,
    /// Safety-net sweep interval in blocks for event-driven mode: even when
    /// no pool event fires, a full scan is forced at least every N blocks.
    pub sweep_interval_blocks: u64,
    /// If true, never broadcast transactions; only log simulated results.
    pub dry_run: bool,
    /// Uniswap QuoterV2 used to price V3 legs off-chain (real tick/liquidity
    /// traversal via eth_call). Defaults to the Base deployment; QUOTER_V2
    /// must be set explicitly for any other chain.
    pub quoter_v2: Address,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let parse_addr = |key: &str| -> Result<Address> {
            let raw = env::var(key).map_err(|_| eyre!("missing env var {key}"))?;
            Address::from_str(&raw).map_err(|e| eyre!("invalid address in {key}: {e}"))
        };

        let rpc_url = env::var("RPC_URL").map_err(|_| eyre!("missing env var RPC_URL"))?;
        let wss_url = env::var("WSS_URL").ok().filter(|s| !s.is_empty());
        let private_key =
            env::var("PRIVATE_KEY").map_err(|_| eyre!("missing env var PRIVATE_KEY"))?;

        let morpho = parse_addr("MORPHO_ADDRESS")?;
        let arb_contract = parse_addr("ARB_CONTRACT")?;
        let loan_token = parse_addr("LOAN_TOKEN")?;
        let quote_token = parse_addr("QUOTE_TOKEN")?;
        // Used to price gas (paid in ETH) into loan-token units.
        let wrapped_native = env::var("WRAPPED_NATIVE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| Address::from_str(&s).map_err(|e| eyre!("invalid WRAPPED_NATIVE: {e}")))
            .transpose()?
            .unwrap_or_else(|| {
                // WETH on Base mainnet.
                Address::from_str("0x4200000000000000000000000000000000000006")
                    .expect("valid constant address")
            });
        if loan_token == quote_token {
            return Err(eyre!("LOAN_TOKEN and QUOTE_TOKEN must differ"));
        }
        // Gas is paid in ETH but profit accrues in the loan token. Only when
        // the loan token IS the wrapped native token can the gas cost be
        // subtracted exactly; for any other loan token there is no trusted
        // on-the-fly conversion, and pretending otherwise turns net-profit
        // filtering into gross-profit filtering. Restrict rather than
        // mislead.
        if loan_token != wrapped_native {
            return Err(eyre!(
                "LOAN_TOKEN must equal WRAPPED_NATIVE ({wrapped_native}); \
                 non-native loans cannot account for gas correctly"
            ));
        }

        // DEX venues as comma-separated entries:
        //   <pair>:<router>[:<kind>[:<fee_bps>[:<factory>[:<stable>[:<fee_tier>[:<pool_id>]]]]]
        // kind: v2 (default) | aero | v3 | v4
        // fee_bps: default 30 (V2/Aero only; V3 uses fee_tier, V4 uses pool_id)
        // factory: default zero (Aero only)
        // stable: default false (Aero only)
        // fee_tier: default 3000 (V3 only, in hundredths of a bip)
        // pool_id: default zero (V4 only, bytes32 hex)
        let venues_raw = env::var("DEX_VENUES").map_err(|_| eyre!("missing env var DEX_VENUES"))?;
        let venues = venues_raw
            .split(',')
            .map(|entry| {
                let entry = entry.trim();
                let mut parts = entry.split(':');
                let pair = parts.next().ok_or_else(|| {
                    eyre!("invalid DEX_VENUES entry '{entry}', expected <pair>:<router>...")
                })?;
                let router = parts
                    .next()
                    .ok_or_else(|| eyre!("invalid DEX_VENUES entry '{entry}', missing router"))?;
                let kind = match parts.next().map(str::trim).unwrap_or("v2") {
                    "v2" => VenueKind::UniswapV2,
                    "aero" => VenueKind::Aerodrome,
                    "v3" => VenueKind::UniswapV3,
                    // V4 reverts on-chain (unlock/lock pattern unsupported);
                    // fail fast at config time instead of at execution.
                    "v4" => {
                        return Err(eyre!(
                            "kind 'v4' in DEX_VENUES '{entry}' is not supported yet"
                        ));
                    }
                    other => return Err(eyre!("invalid kind '{other}' in DEX_VENUES '{entry}'")),
                };
                let fee_bps = parts
                    .next()
                    .map(|s| {
                        s.trim()
                            .parse::<u64>()
                            .map_err(|e| eyre!("invalid fee_bps in DEX_VENUES '{entry}': {e}"))
                    })
                    .transpose()?
                    .unwrap_or(30);
                if fee_bps >= 10_000 {
                    return Err(eyre!("fee_bps {fee_bps} too high in DEX_VENUES '{entry}'"));
                }
                // Optional fields: empty string = default.
                let factory = parts
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| {
                        Address::from_str(s.trim())
                            .map_err(|e| eyre!("invalid factory in DEX_VENUES '{entry}': {e}"))
                    })
                    .transpose()?
                    .unwrap_or(Address::ZERO);
                let stable = parts
                    .next()
                    .map(|s| matches!(s.trim(), "true" | "1" | "yes"))
                    .unwrap_or(false);
                let fee_tier = parts
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| {
                        s.trim()
                            .parse::<u32>()
                            .map_err(|e| eyre!("invalid fee_tier in DEX_VENUES '{entry}': {e}"))
                    })
                    .transpose()?
                    .unwrap_or(3000);
                let pool_id = parts
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| {
                        let s = s.trim().trim_start_matches("0x");
                        let bytes = alloy::hex::decode(s)
                            .map_err(|e| eyre!("invalid pool_id in DEX_VENUES '{entry}': {e}"))?;
                        if bytes.len() != 32 {
                            return Err(eyre!("pool_id must be 32 bytes in DEX_VENUES '{entry}'"));
                        }
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&bytes);
                        Ok::<_, eyre::Report>(arr)
                    })
                    .transpose()?
                    .unwrap_or([0u8; 32]);
                // Optional per-venue QuoterV2 override (V3 only); empty or
                // absent = fall back to the global QUOTER_V2.
                let quoter = parts
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| {
                        Address::from_str(s.trim())
                            .map_err(|e| eyre!("invalid quoter in DEX_VENUES '{entry}': {e}"))
                    })
                    .transpose()?
                    .unwrap_or(Address::ZERO);
                if parts.next().is_some() {
                    return Err(eyre!("too many fields in DEX_VENUES entry '{entry}'"));
                }
                // "auto" = resolve the pool from the factory at startup.
                let pair = if pair.trim().eq_ignore_ascii_case("auto") {
                    if factory == Address::ZERO && kind != VenueKind::Aerodrome {
                        return Err(eyre!(
                            "DEX_VENUES '{entry}': 'auto' pool requires a factory address"
                        ));
                    }
                    Address::ZERO
                } else {
                    Address::from_str(pair.trim())
                        .map_err(|e| eyre!("invalid pair in DEX_VENUES '{entry}': {e}"))?
                };
                Ok::<_, eyre::Report>(Venue {
                    pair,
                    router: Address::from_str(router.trim())
                        .map_err(|e| eyre!("invalid router in DEX_VENUES '{entry}': {e}"))?,
                    kind,
                    fee_bps,
                    factory,
                    stable,
                    fee_tier,
                    pool_id,
                    quoter,
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
                U256::from_str(s.trim()).map_err(|e| eyre!("invalid LOAN_AMOUNTS entry '{s}': {e}"))
            })
            .collect::<Result<Vec<_>>>()?;
        // Morpho Blue rejects zero-asset flash loans.
        if loan_amounts.iter().any(|a| a.is_zero()) {
            return Err(eyre!("LOAN_AMOUNTS must not contain zero"));
        }

        let dry_run = env::var("DRY_RUN")
            .map(|s| matches!(s.as_str(), "1" | "true" | "yes"))
            .unwrap_or(true);

        let min_profit = env::var("MIN_PROFIT")
            .ok()
            .map(|s| U256::from_str(&s))
            .transpose()
            .map_err(|e| eyre!("invalid MIN_PROFIT: {e}"))?
            .unwrap_or(U256::ZERO);
        // A zero floor allows economically meaningless trades (profit of a
        // few wei) that only burn gas. Only tolerable while dry-running.
        if min_profit.is_zero() && !dry_run {
            return Err(eyre!(
                "MIN_PROFIT must be greater than zero when DRY_RUN=false; \
                 set a floor covering at least the expected gas cost"
            ));
        }

        let gas_price_wei = env::var("GAS_PRICE_WEI")
            .ok()
            .map(|s| U256::from_str(&s))
            .transpose()
            .map_err(|e| eyre!("invalid GAS_PRICE_WEI: {e}"))?;

        let slippage_bps = env::var("SLIPPAGE_BPS")
            .ok()
            .map(|s| {
                s.parse::<u64>()
                    .map_err(|e| eyre!("invalid SLIPPAGE_BPS: {e}"))
            })
            .transpose()?
            .unwrap_or(50);
        if slippage_bps >= 10_000 {
            return Err(eyre!("SLIPPAGE_BPS {slippage_bps} too high"));
        }

        let poll_interval_ms = env::var("POLL_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(500);

        // A sweep interval of 0 would disable the safety net entirely;
        // clamp to 1 (sweep every block, i.e. pre-log-trigger behavior).
        let sweep_interval_blocks = env::var("SWEEP_INTERVAL_BLOCKS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(10)
            .max(1);

        // Uniswap QuoterV2 on Base. This address is Base-specific; other
        // chains deploy QuoterV2 elsewhere (e.g. Ethereum mainnet uses
        // 0x61fFE014bA17989E743c5F6cB21bF9697530B21e), so QUOTER_V2 must be
        // set explicitly when targeting a non-Base chain — with a wrong
        // address every V3 quote reverts and V3 venues are silently skipped.
        let quoter_v2 = env::var("QUOTER_V2")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| Address::from_str(&s).map_err(|e| eyre!("invalid QUOTER_V2: {e}")))
            .transpose()?
            .unwrap_or_else(|| {
                Address::from_str("0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a")
                    .expect("valid constant address")
            });

        Ok(Self {
            rpc_url,
            wss_url,
            private_key,
            morpho,
            arb_contract,
            loan_token,
            quote_token,
            wrapped_native,
            venues,
            loan_amounts,
            min_profit,
            gas_price_wei,
            slippage_bps,
            poll_interval_ms,
            sweep_interval_blocks,
            dry_run,
            quoter_v2,
        })
    }
}
