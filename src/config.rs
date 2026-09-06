use alloy::primitives::{Address, U256};
use eyre::{eyre, Result};
use serde::Deserialize;
use std::env;
use std::str::FromStr;

/// Router family of a venue; must match `KIND_*` constants in the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum VenueKind {
    #[serde(rename = "v2")]
    UniswapV2 = 0,
    #[serde(rename = "aero")]
    Aerodrome = 1,
    #[serde(rename = "v3")]
    UniswapV3 = 2,
    #[serde(rename = "v4")]
    UniswapV4 = 3,
    #[serde(rename = "slipstream", alias = "cl")]
    Slipstream = 4,
}

/// One tradable venue: a pool plus its swap router and fee model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
    #[serde(deserialize_with = "deserialize_pool_id")]
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

fn deserialize_pool_id<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    let s = s.trim_start_matches("0x");
    let bytes = alloy::hex::decode(s).map_err(serde::de::Error::custom)?;
    if bytes.len() != 32 {
        return Err(serde::de::Error::custom("pool_id must be 32 bytes"));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

#[derive(Debug, Deserialize)]
struct TomlConfig {
    #[serde(rename = "venues")]
    venues: Vec<Venue>,
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
    /// Re-bootstrap local pool state at most this often (seconds). Event
    /// streams keep state fresh; in polling mode this is the only update
    /// path, so scans pin state no older than one refresh interval.
    pub state_refresh_secs: u64,
    /// Safety-net sweep interval in blocks for event-driven mode: even when
    /// no pool event fires, a full scan is forced at least every N blocks.
    pub sweep_interval_blocks: u64,
    /// Subscribe to `newHeads` over WSS to time safety-net sweeps by block.
    /// Off by default: every header notification is billed by the provider
    /// (Alchemy: 0.04 CU/byte, ~28 CU per 2s block — over a million CU/day),
    /// so sweeps run on a wall-clock timer derived from the sweep interval
    /// instead. Enable when CU cost is irrelevant (e.g. own node).
    pub use_new_heads: bool,
    /// Minimum wall-clock gap between event-driven scans, milliseconds
    /// (0 = no limit). Caps request bursts against RPS-limited RPC plans.
    pub min_scan_interval_ms: u64,
    /// If true, never broadcast transactions; only log simulated results.
    pub dry_run: bool,
    /// Uniswap QuoterV2 used to price V3 legs off-chain (real tick/liquidity
    /// traversal via eth_call). Defaults to the Base deployment; QUOTER_V2
    /// must be set explicitly for any other chain.
    pub quoter_v2: Address,
    /// Aerodrome Slipstream Quoter used to price CL legs. Defaults to the
    /// Base deployment; set QUOTER_SLIPSTREAM explicitly for other chains.
    pub quoter_slipstream: Address,
    /// Read chain state (reserves, quotes, gas) against the `pending` block
    /// tag, i.e. the latest Flashblock preconfirmation (~200ms fresh on Base)
    /// instead of the sealed `latest` block (~2s). Requires a Flashblock-aware
    /// RPC endpoint; the bot probes the endpoint at startup and falls back to
    /// `latest` if `pending` is not meaningfully newer.
    pub use_pending_state: bool,
    /// Submit trades with `eth_sendRawTransactionSync`, which returns a full
    /// receipt within ~200ms (Flashblock inclusion) instead of fire-and-forget
    /// `eth_sendRawTransaction`. The scanner then unblocks for the next
    /// opportunity ~10x sooner instead of waiting for the sealed block. Still
    /// a preconfirmation, not finality — reverts can still cost gas until
    /// Base enables revert protection.
    pub use_flashblock_sync: bool,
    /// Subscribe to `pendingLogs` (Flashblock-level logs) in event-driven
    /// mode, triggering scans ~200ms after a pool event instead of at the
    /// next sealed block. Requires a Flashblock-aware WSS endpoint; falls
    /// back to sealed-block `newHeads`/`logs` if the subscription is refused.
    pub use_pending_logs: bool,
    /// Gate trades by simulating `execute` against the `pending` block tag
    /// (Flashblock state) rather than `latest`. Catches the case where another
    /// Flashblock already moved pool prices before our tx lands, reducing
    /// reverts-on-inclusion. Requires `use_pending_state` semantics; falls
    /// back to `latest` on error.
    pub use_pending_sim: bool,
    /// Estimate gas for `execute` locally with revm instead of
    /// `eth_estimateGas`. The simulation runs in-process against chain state
    /// fetched lazily at the scan's pinned block; any DB/transport error
    /// falls back to the node's `eth_estimateGas`, so enabling this is
    /// strictly additive. Trades one node round-trip for ~tens of lazy
    /// `eth_getStorageAt`/`eth_getCode` fetches on the first simulation of a
    /// block — keep it off on RPS-limited plans.
    pub use_local_sim: bool,
}

impl Config {
    /// Master switch: the single `FLASHBLOCKS` toggle. When false, all four
    /// Flashblock layers are disabled and the bot behaves exactly as it did
    /// pre-Flashblocks (sealed 2s blocks). The per-layer flags are still
    /// parsed so they can be re-enabled by flipping only `FLASHBLOCKS=true`.
    pub fn flashblocks_enabled(&self) -> bool {
        self.use_pending_state
            || self.use_flashblock_sync
            || self.use_pending_logs
            || self.use_pending_sim
    }

    pub fn from_env() -> Result<Self> {
        // ENV_FILE selects an alternate dotenv file (e.g. .env.virtual);
        // unset = default .env lookup, missing file = hard error since the
        // user explicitly asked for it.
        if let Some(path) = env::var("ENV_FILE").ok().filter(|s| !s.is_empty()) {
            dotenvy::from_filename(&path)
                .map_err(|e| eyre!("failed to load ENV_FILE={path}: {e}"))?;
        } else {
            let _ = dotenvy::dotenv();
        }

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

        // DEX venues loaded from config.toml (see [[venues]] tables).
        let config_path = env::var("CONFIG_FILE").unwrap_or_else(|_| "config.toml".to_string());
        let config_text = std::fs::read_to_string(&config_path)
            .map_err(|e| eyre!("failed to read {config_path}: {e}"))?;
        let config: TomlConfig = toml::from_str(&config_text)
            .map_err(|e| eyre!("failed to parse {config_path}: {e}"))?;
        let mut venues = config.venues;
        if venues.len() < 2 {
            return Err(eyre!("config.toml needs at least two venues"));
        }
        for (idx, venue) in venues.iter_mut().enumerate() {
            if venue.fee_bps >= 10_000 {
                return Err(eyre!("venue {idx}: fee_bps {} too high", venue.fee_bps));
            }
            if venue.kind == VenueKind::Slipstream
                && !matches!(venue.fee_tier, 1 | 50 | 100 | 200 | 2000)
            {
                return Err(eyre!(
                    "venue {idx}: slipstream fee_tier must be a tickSpacing \
                     in {{1, 50, 100, 200, 2000}}"
                ));
            }
            if venue.pair.is_zero() {
                if venue.factory.is_zero() && venue.kind != VenueKind::Aerodrome {
                    return Err(eyre!(
                        "venue {idx}: 'auto' pool requires a factory address"
                    ));
                }
            }
            if venue.kind == VenueKind::UniswapV4 {
                return Err(eyre!(
                    "venue {idx}: kind 'v4' in config.toml is not supported yet"
                ));
            }
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
            .map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
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

        // Local pool-state snapshots age: event streams fold every pool
        // event in, but polling mode has no stream, so without a periodic
        // re-bootstrap the scan would price off startup state forever.
        let state_refresh_secs = env::var("STATE_REFRESH_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(60)
            .max(1);

        // A sweep interval of 0 would disable the safety net entirely;
        // clamp to 1 (sweep every block, i.e. pre-log-trigger behavior).
        let sweep_interval_blocks = env::var("SWEEP_INTERVAL_BLOCKS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(10)
            .max(1);

        // newHeads notifications are billed per byte delivered and arrive
        // every ~2s on Base — a steady ~1.2M CU/day on Alchemy just to time
        // sweeps. Default off: the sweep timer is wall-clock based.
        let use_new_heads = env::var("USE_NEW_HEADS")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Minimum wall-clock gap between event-driven scans. Active pairs
        // emit pool events nearly every block (every ~200ms flashblock with
        // USE_PENDING_LOGS), and each scan costs several JSON-RPC requests;
        // without a floor the bot bursts past the RPC plan's RPS limit.
        // Events arriving during the cooldown are not lost: the next scan
        // reads the latest block, which already includes their state
        // changes. Defaults to 2000ms (~0.5 scan/s), which keeps even a
        // 25 RPS plan comfortable; set explicitly to 0 to disable the cap.
        let min_scan_interval_ms = match env::var("MIN_SCAN_INTERVAL_MS") {
            Ok(raw) => raw
                .parse::<u64>()
                .map_err(|e| eyre!("invalid MIN_SCAN_INTERVAL_MS '{raw}': {e}"))?,
            Err(_) => 2000,
        };

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

        // Aerodrome Slipstream Quoter on Base; chain-specific like QUOTER_V2.
        let quoter_slipstream = env::var("QUOTER_SLIPSTREAM")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| Address::from_str(&s).map_err(|e| eyre!("invalid QUOTER_SLIPSTREAM: {e}")))
            .transpose()?
            .unwrap_or_else(|| {
                Address::from_str("0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0")
                    .expect("valid constant address")
            });

        // Flashblock (Base 200ms preconfirmation) options. All default off so
        // a non-Flashblock endpoint behaves exactly as before; enabling them
        // only helps when the RPC/WSS endpoint streams Flashblocks.
        let parse_bool = |key: &str, default: bool| -> Result<bool> {
            env::var(key)
                .ok()
                .filter(|s| !s.is_empty())
                .map(|s| match s.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" => Ok(true),
                    "0" | "false" | "no" => Ok(false),
                    other => Err(eyre!("invalid {key} '{other}', expected 0/1/true/false")),
                })
                .unwrap_or(Ok(default))
        };
        // Reading preconfirmed state is the foundation; the other options
        // (sync submit, pending sim) only make sense against state that is at
        // least as fresh as a Flashblock. Default the state flag on so the
        // single-toggle `FLASHBLOCKS=true` enables the coherent bundle.
        let use_pending_state = parse_bool("USE_PENDING_STATE", true)?;
        let use_flashblock_sync = parse_bool("USE_FLASHBLOCK_SYNC", true)?;
        let use_pending_logs = parse_bool("USE_PENDING_LOGS", true)?;
        // The simulation gate is the most conservative of the four (it adds
        // an eth_call per opportunity); default it off so it must be opted
        // into explicitly to avoid extra RPC cost on RPS-limited plans.
        let use_pending_sim = parse_bool("USE_PENDING_SIM", false)?;
        let use_local_sim = parse_bool("USE_LOCAL_SIM", false)?;
        // Convenience master switch: FLASHBLOCKS=false disables all four at
        // once without touching the individual flags. Keeps .env minimal.
        let flashblocks = parse_bool("FLASHBLOCKS", true)?;
        let (use_pending_state, use_flashblock_sync, use_pending_logs, use_pending_sim) =
            if flashblocks {
                (
                    use_pending_state,
                    use_flashblock_sync,
                    use_pending_logs,
                    use_pending_sim,
                )
            } else {
                (false, false, false, false)
            };

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
            state_refresh_secs,
            sweep_interval_blocks,
            use_new_heads,
            min_scan_interval_ms,
            dry_run,
            quoter_v2,
            quoter_slipstream,
            use_pending_state,
            use_flashblock_sync,
            use_pending_logs,
            use_pending_sim,
            use_local_sim,
        })
    }
}
