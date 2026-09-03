use crate::arbitrage::Opportunity;
use crate::config::{Config, Venue};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::Provider;
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
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
/// `onlyOwner` guard reverts the simulation. When `block` is `pending`, the
/// simulation runs against Flashblock preconfirmed state (~200ms fresh),
/// catching price moves from other Flashblocks before our tx lands.
pub async fn simulate<P: Provider>(
    provider: &P,
    contract: Address,
    owner: Address,
    params: ArbParams,
    block: alloy::eips::BlockId,
) -> Result<()> {
    let arb = IFlashArbitrage::new(contract, provider);
    arb.execute(params).from(owner).block(block).call().await?;
    Ok(())
}

/// Estimate gas for `execute` via eth_estimateGas. When `block` is `pending`,
/// the estimate runs against Flashblock preconfirmed state, so a trade that
/// would revert because a competing Flashblock already moved the pool price
/// is rejected here instead of burning gas on inclusion.
pub async fn estimate_gas<P: Provider>(
    provider: &P,
    contract: Address,
    owner: Address,
    params: ArbParams,
    block: Option<alloy::eips::BlockId>,
) -> Result<U256> {
    let arb = IFlashArbitrage::new(contract, provider);
    let call = arb.execute(params).from(owner);
    let gas = match block {
        Some(b) => call.block(b).estimate_gas().await?,
        None => call.estimate_gas().await?,
    };
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

/// Submit `execute` and return as soon as the node reports Flashblock
/// inclusion (~200ms on a Base Flashblock-aware endpoint). Uses the wallet-
/// enabled provider to build, sign, and broadcast the `execute()` call, then
/// waits for the receipt with a short timeout bound well under one sealed
/// block. On a Flashblock endpoint the node returns the receipt within ~200ms
/// (the synchronous inclusion path); the scanner's in-flight flag is cleared
/// here, so the bot unblocks for the next opportunity ~10x sooner than
/// fire-and-forget. The receipt is a preconfirmation, not finality — it can
/// reorg against the sealed block, so the on-chain `minProfit`/`minOut`
/// backstops must stay in place.
///
/// If the receipt does not arrive within the timeout (slow node, or an
/// endpoint without Flashblocks), the already-broadcast transaction is still
/// pending and must not be replaced. Rather than clear the in-flight flag
/// (which would let the next scan broadcast a competing duplicate), the
/// receipt future is handed to a background watcher — exactly like the
/// asynchronous `execute` path — which clears the flag once the tx lands or
/// fails conclusively. The scan loop resumes immediately, but duplicate
/// protection stays intact.
pub async fn execute_sync<P>(
    provider: P,
    contract: Address,
    params: ArbParams,
    inflight: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<TxHash>
where
    P: Provider + 'static,
{
    use alloy::network::TransactionBuilder;

    let tx = TransactionRequest::default()
        .with_to(contract)
        .with_input(alloy::primitives::Bytes::from(
            IFlashArbitrage::executeCall { params }.abi_encode(),
        ));
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    tracing::debug!(tx = %tx_hash, "execute_sync: broadcast, awaiting flash receipt");

    // Move the receipt future into a background task that owns `pending`
    // (get_receipt takes self by value) and clears the in-flight flag on any
    // conclusive outcome. We then wait on the task's *result* with a short
    // timeout. On a Flashblock endpoint the receipt returns ~200ms and the
    // flag clears here; on timeout the task keeps running and clears the flag
    // later — the already-broadcast tx stays pending and the next scan cannot
    // broadcast a competing duplicate because the flag is still held.
    let flag = inflight.clone();
    let receipt_task = tokio::spawn(async move {
        let receipt = pending.get_receipt().await;
        match &receipt {
            Ok(r) if r.status() => {
                tracing::info!(tx = %r.transaction_hash, "arbitrage transaction flash-confirmed");
            }
            Ok(r) => {
                tracing::warn!(
                    tx = %r.transaction_hash,
                    "arbitrage transaction reverted (gas lost; minProfit backstop held)"
                );
            }
            Err(e) => {
                tracing::warn!(tx = %tx_hash, error = %e, "failed to fetch transaction receipt");
            }
        }
        if let Some(flag) = flag {
            flag.store(false, std::sync::atomic::Ordering::Release);
        }
        receipt
    });

    match tokio::time::timeout(
        std::time::Duration::from_millis(SYNC_RECEIPT_TIMEOUT_MS),
        receipt_task,
    )
    .await
    {
        // Receipt arrived (or a fatal fetch error) within the Flashblock
        // window: the task has already cleared the flag.
        Ok(Ok(Ok(r))) => {
            tracing::debug!(
                tx = %r.transaction_hash,
                status = r.status(),
                "execute_sync: receipt within flash window"
            );
        }
        Ok(Ok(Err(e))) => {
            tracing::debug!(tx = %tx_hash, error = %e, "execute_sync: receipt fetch failed within flash window");
        }
        Ok(Err(e)) => {
            tracing::debug!(tx = %tx_hash, error = %e, "execute_sync: receipt task panicked");
        }
        // Timed out waiting for the receipt. The background task still owns
        // `pending` and the in-flight flag; it will clear the flag when the
        // tx lands or fails conclusively. Resume scanning immediately, but
        // duplicate protection stays intact — no competing broadcast.
        Err(_) => {
            tracing::info!(tx = %tx_hash, "flash-sync receipt timed out; background watcher holds in-flight flag");
        }
    }
    Ok(tx_hash)
}

/// How long `execute_sync` waits for a Flashblock receipt before resuming
/// the scan loop. 200ms is the Flashblock cadence; pad to ~2s (one full
/// block) to absorb jitter on busy blocks while still bounding the wait so a
/// non-Flashblock endpoint cannot stall scanning.
const SYNC_RECEIPT_TIMEOUT_MS: u64 = 2000;

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
            quoter_slipstream: Address::ZERO,
            use_pending_state: false,
            use_flashblock_sync: false,
            use_pending_logs: false,
            use_pending_sim: false,
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

    /// The Slipstream leg kind (4) must round-trip through the alloy binding
    /// and match the contract's `KIND_SLIPSTREAM` constant.
    #[test]
    fn slipstream_leg_kind_round_trips() {
        use crate::config::VenueKind;
        assert_eq!(VenueKind::Slipstream as u8, 4);
        let venue = SwapLeg {
            router: address!("3333333333333333333333333333333333333333"),
            kind: VenueKind::Slipstream as u8,
            factory: Address::ZERO,
            stable: false,
            feeTier: alloy::primitives::Uint::<24, 1>::from(100u32),
            poolId: alloy::primitives::FixedBytes([0u8; 32]),
            minOut: U256::from(900u64),
        };
        let encoded = venue.abi_encode();
        let decoded = SwapLeg::abi_decode(&encoded).expect("leg decodes");
        assert_eq!(decoded.kind, 4);
        assert_eq!(decoded.feeTier, alloy::primitives::Uint::<24, 1>::from(100u32));
    }
}
