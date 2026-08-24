use crate::arbitrage::Opportunity;
use crate::config::{Config, Venue};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::Provider;
use alloy::sol;
use eyre::Result;

sol! {
    struct SwapLeg {
        address router;
        uint8 kind;
        address factory;
        bool stable;
        uint256 minOut;
        uint24 feeTier;
        bytes32 poolId;
    }

    struct ArbParams {
        address token;
        address quote;
        uint256 amount;
        SwapLeg legA;
        SwapLeg legB;
        uint256 minProfit;
    }

    #[sol(rpc)]
    interface IFlashArbitrage {
        function execute(ArbParams params) external;
        function owner() external view returns (address);
    }
}

/// Read the contract owner once at startup; the owner never changes, so
/// per-scan simulations/gas estimates reuse the cached value instead of an
/// extra RPC call per opportunity.
pub async fn fetch_owner<P: Provider>(provider: &P, contract: Address) -> Result<Address> {
    let arb = IFlashArbitrage::new(contract, provider);
    Ok(arb.owner().call().await?)
}

fn build_leg(venue: &Venue, min_out: U256) -> SwapLeg {
    SwapLeg {
        router: venue.router,
        kind: venue.kind as u8,
        factory: venue.factory,
        stable: venue.stable,
        minOut: min_out,
        feeTier: alloy::primitives::Uint::<24, 1>::from(venue.fee_tier),
        poolId: alloy::primitives::FixedBytes(venue.pool_id),
    }
}

/// Scale a simulated output down by the slippage tolerance.
fn with_slippage(expected: U256, slippage_bps: u64) -> U256 {
    expected * U256::from(10_000u64 - slippage_bps) / U256::from(10_000u64)
}

/// Resolve the chosen venue pair to its swap legs and build the calldata.
/// `min_profit` is the on-chain backstop; callers should pass the
/// gas-adjusted threshold (cfg.min_profit + gas cost in loan-token units)
/// so the contract reverts trades that would be unprofitable after gas,
/// instead of letting a gross-positive-but-net-negative trade broadcast
/// and revert later (wasted gas).
pub fn build_params(cfg: &Config, opp: &Opportunity, min_profit: U256) -> ArbParams {
    // Leg 2's input is leg 1's *actual* output, which may legitimately land
    // as low as legA.minOut (= quote_out * (1-s)). Leg 2's output then scales
    // down proportionally to ~amount_out * (1-s), so a single-slippage bound
    // would revert on any further drift even though the trade still clears
    // minProfit. Apply the tolerance twice on leg B so it compounds.
    let leg_a_min = with_slippage(opp.quote_out, cfg.slippage_bps);
    let leg_b_min = with_slippage(
        with_slippage(opp.amount_out, cfg.slippage_bps),
        cfg.slippage_bps,
    );
    ArbParams {
        token: cfg.loan_token,
        quote: cfg.quote_token,
        amount: opp.loan_amount,
        legA: build_leg(&cfg.venues[opp.first], leg_a_min),
        legB: build_leg(&cfg.venues[opp.second], leg_b_min),
        minProfit: min_profit,
    }
}

/// Simulate `execute` via eth_call without broadcasting. The call must carry
/// `from` = the contract owner (cached at startup), otherwise the contract's
/// `onlyOwner` guard reverts the simulation.
pub async fn simulate<P: Provider>(
    provider: &P,
    contract: Address,
    owner: Address,
    params: ArbParams,
) -> Result<()> {
    let arb = IFlashArbitrage::new(contract, provider);
    arb.execute(params).from(owner).call().await?;
    Ok(())
}

/// Estimate gas for `execute` via eth_estimateGas.
pub async fn estimate_gas<P: Provider>(
    provider: &P,
    contract: Address,
    owner: Address,
    params: ArbParams,
) -> Result<U256> {
    let arb = IFlashArbitrage::new(contract, provider);
    let gas = arb.execute(params).from(owner).estimate_gas().await?;
    Ok(U256::from(gas))
}

/// Broadcast `execute` and return as soon as the node accepts the tx,
/// without waiting for inclusion. Waiting for the receipt would block the
/// scan loop for at least one block per trade, blinding the bot to the
/// next opportunity; the receipt is awaited on a background task instead,
/// which logs the outcome (confirmed / reverted) since a revert is
/// protected by the on-chain minProfit backstop and costs only gas. If
/// `inflight` is given, the background watcher clears it on ALL receipt
/// outcomes so the scanner resumes trading once the tx is included.
/// Reuses the caller's wallet-enabled provider instead of opening a fresh
/// connection per trade.
pub async fn execute<P>(
    provider: P,
    contract: Address,
    params: ArbParams,
    inflight: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<TxHash>
where
    P: Provider + 'static,
{
    let arb = IFlashArbitrage::new(contract, provider);
    let pending = arb.execute(params).send().await?;
    let tx_hash = *pending.tx_hash();
    tokio::spawn(async move {
        match pending.get_receipt().await {
            Ok(receipt) if receipt.status() => {
                tracing::info!(tx = %receipt.transaction_hash, "arbitrage transaction confirmed");
            }
            Ok(receipt) => {
                tracing::warn!(
                    tx = %receipt.transaction_hash,
                    "arbitrage transaction reverted on-chain (gas lost; minProfit backstop held)"
                );
            }
            Err(e) => {
                tracing::warn!(tx = %tx_hash, error = %e, "failed to fetch transaction receipt");
            }
        }
        if let Some(flag) = inflight {
            flag.store(false, std::sync::atomic::Ordering::Release);
        }
    });
    Ok(tx_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, U256};
    use alloy::sol_types::{SolCall, SolValue};

    // Canonical calldata for `execute(ArbParams)` produced by the Solidity
    // ABI encoder (`cast calldata`), covering both SwapLeg kinds. Regression
    // guard: the alloy `sol!` binding must decode and re-encode it identically.
    const CANONICAL_CALLDATA: &str = concat!(
        "c0b54622",
        "0000000000000000000000001111111111111111111111111111111111111111",
        "0000000000000000000000002222222222222222222222222222222222222222",
        "0000000000000000000000000000000000000000000000000de0b6b3a7640000",
        "0000000000000000000000003333333333333333333333333333333333333333",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000384",
        "0000000000000000000000000000000000000000000000000000000000000bb8",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000004444444444444444444444444444444444444444",
        "0000000000000000000000000000000000000000000000000000000000000001",
        "0000000000000000000000005555555555555555555555555555555555555555",
        "0000000000000000000000000000000000000000000000000000000000000001",
        "00000000000000000000000000000000000000000000000000000000000003e9",
        "0000000000000000000000000000000000000000000000000000000000000bb8",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000003039",
    );

    #[test]
    fn rust_binding_matches_solidity_calldata() {
        let raw = alloy::hex::decode(CANONICAL_CALLDATA).expect("valid hex");
        let (selector, payload) = raw.split_at(4);
        assert_eq!(
            selector,
            IFlashArbitrage::executeCall::SELECTOR.as_slice(),
            "function selector must match the Solidity ABI"
        );

        let params = ArbParams::abi_decode(payload).expect("solidity payload decodes");
        assert_eq!(
            params.token,
            address!("1111111111111111111111111111111111111111")
        );
        assert_eq!(
            params.quote,
            address!("2222222222222222222222222222222222222222")
        );
        assert_eq!(params.amount, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(
            params.legA.router,
            address!("3333333333333333333333333333333333333333")
        );
        assert_eq!(params.legA.kind, 0);
        assert_eq!(params.legA.factory, Address::ZERO);
        assert!(!params.legA.stable);
        assert_eq!(params.legA.minOut, U256::from(900u64));
        assert_eq!(
            params.legB.router,
            address!("4444444444444444444444444444444444444444")
        );
        assert_eq!(params.legB.kind, 1);
        assert_eq!(
            params.legB.factory,
            address!("5555555555555555555555555555555555555555")
        );
        assert!(params.legB.stable);
        assert_eq!(params.legB.minOut, U256::from(1_001u64));
        assert_eq!(params.minProfit, U256::from(12_345u64));

        assert_eq!(
            params.abi_encode(),
            payload,
            "re-encoding must reproduce the exact Solidity bytes"
        );
    }

    #[test]
    fn build_params_compounds_slippage_on_leg_b() {
        use crate::arbitrage::Opportunity;
        use crate::config::{Config, Venue, VenueKind};

        let venue = |kind| Venue {
            pair: Address::ZERO,
            router: Address::ZERO,
            kind,
            fee_bps: 30,
            factory: Address::ZERO,
            stable: false,
            fee_tier: 3000,
            pool_id: [0u8; 32],
            quoter: Address::ZERO,
        };
        let cfg = Config {
            rpc_url: String::new(),
            wss_url: None,
            private_key: String::new(),
            morpho: Address::ZERO,
            arb_contract: Address::ZERO,
            loan_token: Address::ZERO,
            quote_token: Address::ZERO,
            wrapped_native: Address::ZERO,
            venues: vec![venue(VenueKind::UniswapV2), venue(VenueKind::Aerodrome)],
            loan_amounts: vec![],
            min_profit: U256::ZERO,
            gas_price_wei: None,
            slippage_bps: 50,
            poll_interval_ms: 0,
            sweep_interval_blocks: 10,
            min_scan_interval_ms: 0,
            dry_run: true,
            quoter_v2: Address::ZERO,
        };
        let opp = Opportunity {
            first: 0,
            second: 1,
            loan_amount: U256::from(10_000u64),
            quote_out: U256::from(20_000u64),
            amount_out: U256::from(10_100u64),
            profit: U256::from(100u64),
        };

        let params = build_params(&cfg, &opp, cfg.min_profit);
        // Leg A tolerates one slippage interval: 20000 * 0.995 = 19900.
        assert_eq!(params.legA.minOut, U256::from(19_900u64));
        assert_eq!(params.minProfit, U256::ZERO);
        // Leg B tolerates two compounded intervals (its own input may have
        // drifted down by the leg-A tolerance): floor(10100 * 0.995^2).
        let expected_b = U256::from(10_100u64) * U256::from(9_950u64) / U256::from(10_000u64)
            * U256::from(9_950u64)
            / U256::from(10_000u64);
        assert_eq!(params.legB.minOut, expected_b);
    }
}
