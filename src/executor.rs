use crate::arbitrage::Opportunity;
use crate::config::{Config, Venue};
use alloy::primitives::aliases::I24;
use alloy::primitives::{Address, TxHash, B256, U256, U512};
use alloy::providers::Provider;
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::Result;

/// Thin admin helpers over the alloy `Provider` trait used by the executor's
/// explicit-fill broadcast path (`execute_sync`). Keeping them here (instead
/// of inlining trait calls) makes the two paths' fee/gas/nonce semantics
/// identical and unit-testable.
pub mod admin {
    use super::*;
    use alloy::providers::Provider;
    use alloy::rpc::types::eth::TransactionRequest;

    /// [`Provider::estimate_eip1559_fees`] — named alias so the call site
    /// reads as a fee ESTIMATION and stays greppable.
    pub async fn estimate_eip1559_fees<P: Provider>(
        provider: &P,
    ) -> eyre::Result<alloy::eips::eip1559::Eip1559Estimation> {
        provider
            .estimate_eip1559_fees()
            .await
            .map_err(eyre::Error::from)
    }

    /// [`Provider::estimate_gas`] pinned to the network default block.
    pub async fn estimate_gas<P: Provider>(
        provider: &P,
        tx: &TransactionRequest,
    ) -> eyre::Result<u64> {
        provider
            .estimate_gas(tx.clone())
            .await
            .map_err(eyre::Error::from)
    }

    /// Current nonce of `address` against the `pending` tag — the exact count
    /// the node would assign our soon-to-be-next tx.
    pub async fn get_transaction_count<P: Provider>(
        provider: &P,
        address: Address,
    ) -> eyre::Result<u64> {
        provider
            .get_transaction_count(address)
            .block_id(alloy::eips::BlockId::pending())
            .await
            .map_err(eyre::Error::from)
    }
}

/// Error carrying the account mismatch that made a trade unsafe to broadcast.
///
/// Simulations/gas estimates must run `from` the contract owner and the
/// broadcast wallet must BE the owner (`onlyOwner`), so the executor keeps a
/// (possibly refreshed) copy of the on-chain owner. When that owner is not
/// the wallet the bot signs with, every simulation silently reverts
/// `NotOwner` while broadcasts would be rejected too — stop with a
/// descriptive error instead of scanning against a stale owner.
#[derive(Debug, Clone)]
pub struct OwnershipMismatch {
    /// Owner read from the contract (possibly refreshed after a transfer).
    pub owner: Address,
    /// Signer address of the configured `PRIVATE_KEY`.
    pub signer: Address,
}

impl std::fmt::Display for OwnershipMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "contract owner {} != bot signing wallet {}: the FlashArbitrage \
             contract has been transferred to a key the bot does not hold. \
             Point PRIVATE_KEY at the new owner key and restart the bot in \
             coordination with the on-chain ownership transfer",
            self.owner, self.signer
        )
    }
}

impl std::error::Error for OwnershipMismatch {}

sol! {
    struct SwapLeg {
        address router;
        uint8 kind;
        address factory;
        bool stable;
        uint256 minOut;
        uint24 feeTier;
        bytes32 poolId;
        int24 tickSpacing;
        address hooks;
        bool zeroForOne;
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

        event ArbExecuted(
            address indexed token,
            address indexed quote,
            uint256 amount,
            uint256 profit
        );
    }
}

/// Read the contract's current owner. Called at startup and re-checked on a
/// timer: the contract supports two-step ownership transfer at runtime, so
/// a stale startup-only value would keep simulations running `from` the
/// former owner and reject every candidate after a transfer. The caller
/// throttles this to one extra eth_call per `owner_refresh_secs`.
pub async fn fetch_owner<P: Provider>(provider: &P, contract: Address) -> Result<Address> {
    let arb = IFlashArbitrage::new(contract, provider);
    Ok(arb.owner().call().await?)
}

/// Build one swap leg. For V4 venues the swap direction is NOT a per-venue
/// constant: the same pool sells the loan token as leg A and the quote token
/// as leg B, and the contract's reconstructed PoolKey orders currencies by
/// address (`currency0 = min(from, to)`), so a leg sells currency0 exactly
/// when its input token sorts below its output token. Derive `zeroForOne`
/// from that per-leg ordering; the configured `venue.zero_for_one` is
/// ignored for V4 (it is kept for DEX_VENUES/TOML parity and non-V4 kinds,
/// where the contract never reads it).
fn build_leg(venue: &Venue, min_out: U256, token_in: Address, token_out: Address) -> SwapLeg {
    let zero_for_one = if venue.kind == crate::config::VenueKind::UniswapV4 {
        token_in < token_out
    } else {
        venue.zero_for_one
    };
    SwapLeg {
        router: venue.router,
        kind: venue.kind as u8,
        factory: venue.factory,
        stable: venue.stable,
        minOut: min_out,
        feeTier: alloy::primitives::Uint::<24, 1>::from(venue.fee_tier),
        poolId: alloy::primitives::FixedBytes(venue.pool_id),
        tickSpacing: I24::try_from(i64::from(venue.tick_spacing)).expect("tick_spacing fits int24"),
        hooks: venue.hooks,
        zeroForOne: zero_for_one,
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
///
/// Leg A applies the configured tolerance to its quoted output; leg B
/// COMPOUNDS the two independent adverse moves it can experience. Leg B's
/// input is leg A's *actual* output, which can land as low as `legA.minOut`,
/// and then the second pool can itself move against the trade. The leg-B
/// bound is therefore the nominal leg-B output scaled down by leg A's
/// worst case (`leg_a_min / quote_out`) and then by leg B's own tolerance.
/// A single flat tolerance on the nominal output would let both pools drift
/// within their limits while the real output falls below `minOut`, reverting
/// the second router call (`Too little received`) before the final on-chain
/// `minProfit` check ever runs. `minProfit` stays the profitability backstop
/// after both legs.
pub fn build_params(cfg: &Config, opp: &Opportunity, min_profit: U256) -> ArbParams {
    let leg_a_min = with_slippage(opp.quote_out, cfg.slippage_bps);
    // Leg B's input is leg A's actual output; scale the nominal leg-B output
    // down by leg A's worst case before applying leg B's own tolerance. When
    // the quoted output is zero (defensive; valid opportunities always have a
    // positive quote) fall back to the raw amount_out — the compounded bound
    // degenerates harmlessly.
    //
    // The whole scaling chain can overflow U256 even though every FINAL
    // value fits: `amount_out` reaches ~2^250 for a deep-recollateralized
    // loan, so `amount_out * leg_a_min` needs ~378 bits and the intermediate
    // quotient (~2^250) times the 9950 tolerance still needs ~263 bits.
    // Run the two-step scaling in U512 and only clamp to U256 at the end.
    //
    // The final bound is mathematically at most `amount_out` (leg_a_min <=
    // quote_out and the tolerance is a strict discount), so the checked
    // conversion cannot fail.
    let leg_b_min = if opp.quote_out.is_zero() {
        // Defensive fallback (valid opportunities always have a positive
        // quote); the single-width tolerance can hit the same wrap only on
        // this unreachable path.
        with_slippage(opp.amount_out, cfg.slippage_bps)
    } else {
        let product = opp.amount_out.widening_mul(leg_a_min);
        let quotient = product / U512::from(opp.quote_out);
        let tolerance = U512::from(10_000u64 - cfg.slippage_bps);
        (quotient * tolerance / U512::from(10_000u64)).to::<U256>()
    };
    ArbParams {
        token: cfg.loan_token,
        quote: cfg.quote_token,
        amount: opp.loan_amount,
        legA: build_leg(
            &cfg.venues[opp.first],
            leg_a_min,
            cfg.loan_token,
            cfg.quote_token,
        ),
        legB: build_leg(
            &cfg.venues[opp.second],
            leg_b_min,
            cfg.quote_token,
            cfg.loan_token,
        ),
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

/// Estimate gas for `execute` locally via revm instead of eth_estimateGas.
/// The call is simulated against chain state at `block` fetched lazily
/// through `provider`; the outcome mirrors eth_estimateGas: success carries
/// gas used, a revert is reported as [`SimOutcome::Reverted`] so the caller
/// can skip the opportunity, and transport/DB problems surface as `Err` for
/// RPC fallback.
pub async fn estimate_gas_local<P: Provider>(
    provider: P,
    contract: Address,
    owner: Address,
    params: ArbParams,
    block: alloy::eips::BlockId,
) -> Result<crate::sim::SimOutcome> {
    // Block/chain context from the same block the state reads pin to. A
    // failed fetch is an Err so the caller falls back to eth_estimateGas —
    // executing with a default context against pinned state can diverge
    // from the node's verdict.
    let env = crate::sim::fetch_sim_env(&provider, block).await?;
    let calldata = IFlashArbitrage::executeCall { params }.abi_encode().into();
    crate::sim::simulate_call(provider, contract, owner, calldata, block, Some(env))
}

/// Outcome of inspecting a mined `execute` receipt.
///
/// A receipt `status == true` only says the *transaction* did not revert; it
/// does not prove the arbitrage did anything. `ArbExecuted` is the contract's
/// only signal that the flash-loan callback ran, both legs swapped, the loan
/// was repaid and profit (if any) was swept. Treating a status-only success as
/// confirmation would report a successful trade for a transaction that moved
/// nothing (a wrong/degraded `morpho`, or a contract deployed before the
/// `CallbackNotInvoked` guard existed).
#[derive(Debug, PartialEq, Eq)]
pub enum ReceiptVerdict {
    /// `ArbExecuted` found for `expected_token` from `contract`, with the
    /// on-chain profit.
    Confirmed {
        token: Address,
        amount: U256,
        profit: U256,
    },
    /// Transaction reverted; `gas lost` — the on-chain backstops did their job.
    Reverted,
    /// Mined successfully but the contract never reported the arb cycle.
    MissingEvent,
}

impl std::fmt::Display for ReceiptVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiptVerdict::Confirmed { amount, profit, .. } => {
                write!(f, "confirmed: ArbExecuted amount={amount} profit={profit}")
            }
            ReceiptVerdict::Reverted => write!(f, "reverted on-chain (gas lost)"),
            ReceiptVerdict::MissingEvent => write!(
                f,
                "mined without ArbExecuted: the flash-loan callback never ran \
                 (wrong morpho address or a contract predating the \
                 CallbackNotInvoked guard)"
            ),
        }
    }
}

/// Decode the `ArbExecuted` log that `contract` emits for `expected_token`.
///
/// Scanning logs (instead of matching `topics[0]` by hand) validates the
/// emitter and the indexed `token` at the same time, so a look-alike event
/// from another address cannot be mistaken for confirmation. Returns `None`
/// when no matching log exists.
pub fn decode_arb_executed(
    logs: &[alloy::rpc::types::eth::Log],
    contract: Address,
    expected_token: Address,
) -> Option<(U256, U256)> {
    logs.iter().find_map(|log| {
        if log.address() != contract {
            return None;
        }
        match log.log_decode::<IFlashArbitrage::ArbExecuted>() {
            Ok(decoded) if decoded.inner.data.token == expected_token => {
                Some((decoded.inner.data.amount, decoded.inner.data.profit))
            }
            _ => None,
        }
    })
}

/// Classify a broadcast receipt: confirmed only when the contract's
/// `ArbExecuted` event is present for the loan token that was requested.
pub fn verdict_from_receipt(
    receipt: &alloy::rpc::types::eth::TransactionReceipt,
    contract: Address,
    expected_token: Address,
) -> ReceiptVerdict {
    if !receipt.status() {
        return ReceiptVerdict::Reverted;
    }
    match decode_arb_executed(receipt.logs(), contract, expected_token) {
        Some((amount, profit)) => ReceiptVerdict::Confirmed {
            token: expected_token,
            amount,
            profit,
        },
        None => ReceiptVerdict::MissingEvent,
    }
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
    let expected_token = params.token;
    let pending = arb.execute(params).send().await?;
    let tx_hash = *pending.tx_hash();
    tokio::spawn(async move {
        match pending.get_receipt().await {
            Ok(receipt) => match verdict_from_receipt(&receipt, contract, expected_token) {
                ReceiptVerdict::Confirmed { amount, profit, .. } => {
                    tracing::info!(
                        tx = %receipt.transaction_hash,
                        amount = %amount,
                        profit = %profit,
                        "arbitrage transaction confirmed"
                    );
                }
                ReceiptVerdict::Reverted => {
                    tracing::warn!(
                        tx = %receipt.transaction_hash,
                        "arbitrage transaction reverted on-chain (gas lost; minProfit backstop held)"
                    );
                }
                // Mined, but the contract never reported the cycle. This is
                // the silent no-op the CallbackNotInvoked guard now blocks on
                // new deployments; for an older deployment it is the only
                // symptom, so surface it loudly instead of as a success.
                ReceiptVerdict::MissingEvent => {
                    tracing::error!(
                        tx = %receipt.transaction_hash,
                        contract = %contract,
                        token = %expected_token,
                        "arbitrage transaction mined WITHOUT ArbExecuted: the \
                         flash-loan callback never ran, no trade happened. Check \
                         that MORPHO/ARB_CONTRACT point at the real Morpho Blue \
                         and the arb contract, and redeploy if the contract \
                         predates the CallbackNotInvoked guard"
                    );
                }
            },
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
///
/// `expected_pending_hash`: when the caller scanned preconfirmed `pending`
/// state, it snapshots the pending block hash the scan was priced against;
/// this function re-reads the pending hash after the fee/gas/nonce fills
/// and refuses to broadcast if a Flashblock advanced the state in the
/// meantime (audit finding #5). `None` disables the check (sealed scans).
pub async fn execute_sync<P>(
    provider: P,
    contract: Address,
    params: ArbParams,
    signer: Address,
    inflight: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    expected_pending_hash: Option<B256>,
) -> Result<TxHash>
where
    P: Provider + 'static,
{
    use alloy::network::TransactionBuilder;

    let expected_token = params.token;
    let calldata =
        alloy::primitives::Bytes::from(IFlashArbitrage::executeCall { params }.abi_encode());
    // The sync submit path must set fee, gas and nonce fields explicitly:
    // alloy's `send_transaction` on a wallet-enabled provider performs no
    // automatic fee/gas estimation for a bare `TransactionRequest`, and a
    // Flashblock race means a tx that fails to include at the current
    // basefee simply waits — the 200ms synchronous receipt then times out,
    // the already-broadcast tx staying pending and stalling the nonce while
    // the next scan is already allowed to broadcast a duplicate (distinct
    // nonce) that reverts. Estimate EIP-1559 fees + gas exactly like the
    // fire-and-forget `execute` path does, fill the nonce explicitly so a
    // re-simulated duplicate can never double-spend, and pad the gas limit
    // (1.33x) to cover estimation variance between scan and inclusion
    // (audit finding #2). The L1 data fee is accounted separately in the
    // caller's gas/profit math, not inside the gas limit.
    let fee_est = admin::estimate_eip1559_fees(&provider).await?;
    // Alloy's estimator and `TransactionRequest` fee fields are `u128`, so
    // the broadcast tx can carry fees wider than eight bytes; the L1 oracle
    // size estimate (`unsigned_tx_rlp_len`) reserves 16 payload bytes for
    // each fee field to stay a true upper bound on that tx.
    // The gas estimate must run `from` the signing wallet: `execute` is
    // `onlyOwner`, so without the sender the estimation uses the RPC default
    // and reverts `NotOwner` before the wallet-backed broadcast can happen.
    // Keep the sender on the final transaction so `eth_estimateGas` and
    // `send_transaction` agree on the caller.
    let est_tx = TransactionRequest::default()
        .with_to(contract)
        .with_input(calldata.clone())
        .with_from(signer)
        .with_max_fee_per_gas(fee_est.max_fee_per_gas)
        .with_max_priority_fee_per_gas(fee_est.max_priority_fee_per_gas);
    let gas_limit = admin::estimate_gas(&provider, &est_tx).await?;
    let nonce = admin::get_transaction_count(&provider, signer).await?;
    // Pre-submit state-advancement guard inside the submit path itself (audit
    // finding #5). The scan's final guard runs before building the tx, but
    // fee/gas/nonce estimation between that guard and `.send()` is itself
    // three RPC round-trips — on a Flashblock endpoint the pending state can
    // advance within them, and the priced legs would land against a state
    // they were not priced against. Re-check the pending hash right before
    // sending and refuse to broadcast when a new Flashblock landed. This
    // closes the remaining window (guard→submit) that the scan-side guards
    // cannot see.
    if let Some(expected) = expected_pending_hash {
        let current = provider
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Pending)
            .await?
            .map(|b| b.header.hash);
        if matches!(current, Some(h) if h != expected) {
            return Err(eyre::eyre!(
                "pending state advanced during execute_sync fee/gas/nonce fill; \
                 refusing to broadcast against stale legs"
            ));
        }
    }
    let tx = est_tx
        .with_gas_limit((gas_limit as f64 * 1.33) as u64)
        .with_nonce(nonce);
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
            Ok(r) => match verdict_from_receipt(r, contract, expected_token) {
                ReceiptVerdict::Confirmed { amount, profit, .. } => {
                    tracing::info!(
                        tx = %r.transaction_hash,
                        amount = %amount,
                        profit = %profit,
                        "arbitrage transaction flash-confirmed"
                    );
                }
                ReceiptVerdict::Reverted => {
                    tracing::warn!(
                        tx = %r.transaction_hash,
                        "arbitrage transaction reverted (gas lost; minProfit backstop held)"
                    );
                }
                ReceiptVerdict::MissingEvent => {
                    tracing::error!(
                        tx = %r.transaction_hash,
                        contract = %contract,
                        token = %expected_token,
                        "arbitrage transaction mined WITHOUT ArbExecuted: the \
                         flash-loan callback never ran, no trade happened. Check \
                         that MORPHO/ARB_CONTRACT point at the real Morpho Blue \
                         and the arb contract, and redeploy if the contract \
                         predates the CallbackNotInvoked guard"
                    );
                }
            },
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
    // ABI encoder for the 10-field SwapLeg struct (V4 fields included),
    // covering both leg kinds. Regression guard: the alloy `sol!` binding must
    // decode and re-encode it identically, and each field must land in the
    // position the contract decodes.
    //
    // Field layout per SwapLeg (10 words):
    //   router, kind(+pad), factory, stable(+pad), minOut,
    //   feeTier(+pad), poolId(32 bytes), tickSpacing(+pad), hooks, zeroForOne
    const CANONICAL_CALLDATA: &str = concat!(
        // execute((address,address,uint256,(address,uint8,address,bool,uint256,
        //          uint24,bytes32,int24,address,bool),(...same...),uint256)),
        // produced by the alloy Solidity ABI encoder (matches the on-chain
        // selector and the 10-field SwapLeg layout).
        "c7828930",
        "0000000000000000000000001111111111111111111111111111111111111111", // token
        "0000000000000000000000002222222222222222222222222222222222222222", // quote
        "0000000000000000000000000000000000000000000000000de0b6b3a7640000", // amount
        "0000000000000000000000003333333333333333333333333333333333333333", // legA.router
        "0000000000000000000000000000000000000000000000000000000000000000", // legA.kind(v2)+pad
        "0000000000000000000000000000000000000000000000000000000000000000", // legA.factory(zero)
        "0000000000000000000000000000000000000000000000000000000000000000", // legA.stable(false)+pad
        "0000000000000000000000000000000000000000000000000000000000000384", // legA.minOut(900)
        "0000000000000000000000000000000000000000000000000000000000000bb8", // legA.feeTier(3000)+pad
        "0000000000000000000000000000000000000000000000000000000000000000", // legA.poolId(zeros)
        "000000000000000000000000000000000000000000000000000000000000003c", // legA.tickSpacing(60)
        "0000000000000000000000000000000000000000000000000000000000000000", // legA.hooks(zero)
        "0000000000000000000000000000000000000000000000000000000000000000", // legA.zeroForOne(false)+pad
        "0000000000000000000000004444444444444444444444444444444444444444", // legB.router
        "0000000000000000000000000000000000000000000000000000000000000001", // legB.kind(aero)+pad
        "0000000000000000000000005555555555555555555555555555555555555555", // legB.factory
        "0000000000000000000000000000000000000000000000000000000000000001", // legB.stable(true)+pad
        "00000000000000000000000000000000000000000000000000000000000003e9", // legB.minOut(1001)
        "00000000000000000000000000000000000000000000000000000000000001f4", // legB.feeTier(500)+pad
        "1111111111111111111111111111111111111111111111111111111111111111", // legB.poolId(0x11..)
        "0000000000000000000000000000000000000000000000000000000000000078", // legB.tickSpacing(120)
        "0000000000000000000000006666666666666666666666666666666666666666", // legB.hooks
        "0000000000000000000000000000000000000000000000000000000000000001", // legB.zeroForOne(true)+pad
        "0000000000000000000000000000000000000000000000000000000000003039", // minProfit(12345)
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
        // Leg A (v2): V4 fields must decode as their zero/defaults.
        assert_eq!(params.legA.tickSpacing, I24::try_from(60_i64).unwrap());
        assert_eq!(
            params.legA.feeTier,
            alloy::primitives::Uint::<24, 1>::from(3000u32)
        );
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
        // Leg B (aero): V4 fields carry explicit values in the canonical bytes.
        assert_eq!(
            params.legB.feeTier,
            alloy::primitives::Uint::<24, 1>::from(500u32)
        );
        assert_eq!(
            params.legB.poolId,
            alloy::primitives::FixedBytes([0x11; 32])
        );
        assert_eq!(params.legB.tickSpacing, I24::try_from(120_i64).unwrap());
        assert_eq!(
            params.legB.hooks,
            address!("6666666666666666666666666666666666666666")
        );
        assert!(params.legB.zeroForOne);
        assert_eq!(params.minProfit, U256::from(12_345u64));

        assert_eq!(
            params.abi_encode(),
            payload,
            "re-encoding must reproduce the exact Solidity bytes"
        );
    }

    #[test]
    fn build_params_compounds_leg_b_slippage() {
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
            tick_spacing: 60,
            hooks: Address::ZERO,
            zero_for_one: false,
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
            owner_refresh_secs: 60,
            poll_interval_ms: 0,
            state_refresh_secs: 60,
            sweep_interval_blocks: 10,
            use_new_heads: false,
            min_scan_interval_ms: 0,
            dry_run: true,
            quoter_v2: Address::ZERO,
            quoter_slipstream: Address::ZERO,
            quoter_v4: Address::ZERO,
            use_pending_state: false,
            use_flashblock_sync: false,
            use_pending_logs: false,
            use_pending_sim: false,
            use_local_sim: false,
        };
        let opp = Opportunity {
            first: 0,
            second: 1,
            loan_amount: U256::from(10_000u64),
            quote_out: U256::from(20_000u64),
            amount_out: U256::from(10_100u64),
            profit: U256::from(100u64),
            leg1_local: false,
            leg2_local: false,
        };

        let params = build_params(&cfg, &opp, cfg.min_profit);
        // Leg A tolerates one slippage interval: 20000 * 0.995 = 19900.
        assert_eq!(params.legA.minOut, U256::from(19_900u64));
        assert_eq!(params.minProfit, U256::ZERO);
        // Leg B compounds BOTH independent adverse moves: its input is leg
        // A's actual output (worst case leg_a_min = 19900, i.e. * 0.995) and
        // the second pool can itself drift by one interval (* 0.995). Bound
        // = 10_100 * (19900 / 20000) * 0.995, floored by integer math.
        let leg_b_worst_case = U256::from(10_100u64) * U256::from(19_900u64)
            / U256::from(20_000u64)
            * U256::from(9_950u64)
            / U256::from(10_000u64);
        assert_eq!(leg_b_worst_case, U256::from(9_998u64));
        assert_eq!(params.legB.minOut, leg_b_worst_case);
    }

    /// The same adverse move in both legs (leg A priced leg B's input at the
    /// nominal output, leg B executed against leg A's actual, lower output)
    /// must not let the compounded bound fall below a single-tolerance
    /// bound — the review regression that dropped the second leg's guard.
    #[test]
    fn build_params_leg_b_min_out_never_below_single_tolerance() {
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
            tick_spacing: 60,
            hooks: Address::ZERO,
            zero_for_one: false,
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
            owner_refresh_secs: 60,
            poll_interval_ms: 0,
            state_refresh_secs: 60,
            sweep_interval_blocks: 10,
            use_new_heads: false,
            min_scan_interval_ms: 0,
            dry_run: true,
            quoter_v2: Address::ZERO,
            quoter_slipstream: Address::ZERO,
            quoter_v4: Address::ZERO,
            use_pending_state: false,
            use_flashblock_sync: false,
            use_pending_logs: false,
            use_pending_sim: false,
            use_local_sim: false,
        };
        let opp = Opportunity {
            first: 0,
            second: 1,
            loan_amount: U256::from(10_000u64),
            quote_out: U256::from(20_000u64),
            amount_out: U256::from(10_100u64),
            profit: U256::from(100u64),
            leg1_local: false,
            leg2_local: false,
        };

        let params = build_params(&cfg, &opp, cfg.min_profit);
        // The compounded bound must always be at or below the single
        // tolerance bound for the same legs.
        let single = opp.amount_out * U256::from(9_950u64) / U256::from(10_000u64);
        assert!(params.legB.minOut <= single, "compounded bound is tighter");
        // And it must be strictly tighter when both legs move (slippage > 0
        // and both quotes positive).
        assert!(params.legB.minOut < single);
    }

    /// Regression for the review finding that `amount_out * leg_a_min /
    /// quote_out` multiplied in U256: a deep-recollateralized loan pushes
    /// `amount_out` (~2^250) and `leg_a_min` (~2^128) to a product needing
    /// ~378 bits, which overflows U256 (panicking in debug, wrapping the
    /// bound in release) even though the scaled-down quotient fits. The
    /// multiplication must widen into U512 before the division.
    #[test]
    fn build_params_leg_b_scaling_handles_u512_overflow() {
        use crate::arbitrage::Opportunity;
        use crate::config::{Config, Venue, VenueKind};
        use std::str::FromStr;

        let venue = |kind| Venue {
            pair: Address::ZERO,
            router: Address::ZERO,
            kind,
            fee_bps: 30,
            factory: Address::ZERO,
            stable: false,
            fee_tier: 3000,
            pool_id: [0u8; 32],
            tick_spacing: 60,
            hooks: Address::ZERO,
            zero_for_one: false,
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
            owner_refresh_secs: 60,
            poll_interval_ms: 0,
            state_refresh_secs: 60,
            sweep_interval_blocks: 10,
            use_new_heads: false,
            min_scan_interval_ms: 0,
            dry_run: true,
            quoter_v2: Address::ZERO,
            quoter_slipstream: Address::ZERO,
            quoter_v4: Address::ZERO,
            use_pending_state: false,
            use_flashblock_sync: false,
            use_pending_logs: false,
            use_pending_sim: false,
            use_local_sim: false,
        };
        let opp = Opportunity {
            first: 0,
            second: 1,
            loan_amount: U256::from(10_000u64),
            quote_out: U256::from(1u64) << 128,
            amount_out: U256::from(1u64) << 250,
            profit: U256::from(1u64),
            leg1_local: false,
            leg2_local: false,
        };

        // Premise check: the 256-bit intermediate genuinely overflows.
        let leg_a_min = with_slippage(opp.quote_out, cfg.slippage_bps);
        let product = opp.amount_out.widening_mul(leg_a_min);
        assert!(
            product > U512::from(U256::MAX),
            "test premise: amount_out * leg_a_min must exceed U256::MAX"
        );

        // Expected bound computed independently in 512-bit space:
        // quotient = floor(2^250 * leg_a_min / 2^128), then leg B's own
        // tolerance applied once. Hard-coded to pin the exact floor.
        let expected =
            U256::from_str("0x3f5c91d14e3bcd35a858793dd97f62b680a3d70a3d70a3d70a3d70a3d70a3d7")
                .unwrap();
        let params = build_params(&cfg, &opp, cfg.min_profit);
        assert_eq!(params.legB.minOut, expected);
        // And the bound stays a strict (conservative) discount vs the
        // nominal leg-B output.
        assert!(params.legB.minOut < opp.amount_out);
    }

    /// The V4 direction is NOT a per-venue constant: the same pool sells the
    /// loan token as leg A and the quote token as leg B, which the contract's
    /// PoolKey reconstruction (currencies sorted by address) requires to
    /// have opposite `zeroForOne` flags. Putting one V4 venue in both
    /// positions must derive opposite directions from each leg's own input
    /// token — never from the once-per-venue config field.
    #[test]
    fn build_params_derives_opposite_v4_direction_per_leg() {
        use crate::arbitrage::Opportunity;
        use crate::config::{Config, Venue, VenueKind};

        let v4 = Venue {
            pair: Address::ZERO,
            router: Address::ZERO,
            kind: VenueKind::UniswapV4,
            fee_bps: 30,
            factory: Address::ZERO,
            stable: false,
            fee_tier: 3000,
            pool_id: [0u8; 32],
            tick_spacing: 60,
            hooks: Address::ZERO,
            zero_for_one: false, // configured value must be derived away
            quoter: Address::ZERO,
        };
        let cfg = Config {
            rpc_url: String::new(),
            wss_url: None,
            private_key: String::new(),
            morpho: Address::ZERO,
            arb_contract: Address::ZERO,
            // loan_token < quote_token numerically, so currency0 = loan_token.
            loan_token: address!("1000000000000000000000000000000000000001"),
            quote_token: address!("2000000000000000000000000000000000000002"),
            wrapped_native: Address::ZERO,
            venues: vec![v4, v4],
            loan_amounts: vec![],
            min_profit: U256::ZERO,
            gas_price_wei: None,
            slippage_bps: 50,
            owner_refresh_secs: 60,
            poll_interval_ms: 0,
            state_refresh_secs: 60,
            sweep_interval_blocks: 10,
            use_new_heads: false,
            min_scan_interval_ms: 0,
            dry_run: true,
            quoter_v2: Address::ZERO,
            quoter_slipstream: Address::ZERO,
            quoter_v4: Address::ZERO,
            use_pending_state: false,
            use_flashblock_sync: false,
            use_pending_logs: false,
            use_pending_sim: false,
            use_local_sim: false,
        };
        let opp = Opportunity {
            first: 0,
            second: 1,
            loan_amount: U256::from(10_000u64),
            quote_out: U256::from(20_000u64),
            amount_out: U256::from(10_100u64),
            profit: U256::from(100u64),
            leg1_local: false,
            leg2_local: false,
        };

        let params = build_params(&cfg, &opp, cfg.min_profit);
        // Leg A sells the loan token (currency0 here), leg B sells the quote
        // token (currency1 here), so the flags must be opposite.
        assert!(params.legA.zeroForOne, "leg A sells currency0 (loan token)");
        assert!(
            !params.legB.zeroForOne,
            "leg B sells currency1 (quote token)"
        );
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
            tickSpacing: I24::try_from(60_i64).unwrap(),
            hooks: Address::ZERO,
            zeroForOne: false,
        };
        let encoded = venue.abi_encode();
        let decoded = SwapLeg::abi_decode(&encoded).expect("leg decodes");
        assert_eq!(decoded.kind, 4);
        assert_eq!(
            decoded.feeTier,
            alloy::primitives::Uint::<24, 1>::from(100u32)
        );
    }

    /// The V4 leg fields (kind 3) must round-trip and carry tickSpacing/hooks/
    /// zeroForOne, matching the contract's extended SwapLeg struct.
    #[test]
    fn v4_leg_fields_round_trip() {
        use crate::config::VenueKind;
        assert_eq!(VenueKind::UniswapV4 as u8, 3);
        let venue = SwapLeg {
            router: address!("4444444444444444444444444444444444444444"),
            kind: VenueKind::UniswapV4 as u8,
            factory: Address::ZERO,
            stable: false,
            feeTier: alloy::primitives::Uint::<24, 1>::from(500u32),
            poolId: alloy::primitives::FixedBytes([0xab; 32]),
            minOut: U256::from(777u64),
            tickSpacing: I24::try_from(120_i64).unwrap(),
            hooks: address!("5555555555555555555555555555555555555555"),
            zeroForOne: true,
        };
        let encoded = venue.abi_encode();
        let decoded = SwapLeg::abi_decode(&encoded).expect("leg decodes");
        assert_eq!(decoded.kind, 3);
        assert_eq!(decoded.poolId.0, [0xab; 32]);
        assert_eq!(decoded.tickSpacing, I24::try_from(120_i64).unwrap());
        assert_eq!(
            decoded.hooks,
            address!("5555555555555555555555555555555555555555")
        );
        assert!(decoded.zeroForOne);
        assert_eq!(
            decoded.feeTier,
            alloy::primitives::Uint::<24, 1>::from(500u32)
        );
    }

    // --- receipt verdict (audit finding: status-only success) ---
    //
    // A receipt must not be reported as a successful trade unless the contract
    // emitted `ArbExecuted` for the requested loan token. Build the receipts
    // with the real alloy types (consensus receipt + rpc log) so these tests
    // exercise the same decode path a live receipt takes.

    use alloy::sol_types::SolEvent;

    /// `ReceiptEnvelope` has no `Default`, so build the EIP-1559 variant.
    fn envelope(
        status: bool,
        logs: Vec<alloy::rpc::types::eth::Log>,
    ) -> alloy::consensus::ReceiptEnvelope<alloy::rpc::types::eth::Log> {
        use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
        ReceiptEnvelope::Eip1559(ReceiptWithBloom::<Receipt<_>> {
            receipt: Receipt {
                status: Eip658Value::Eip658(status),
                cumulative_gas_used: 0,
                logs,
            },
            logs_bloom: Default::default(),
        })
    }

    fn receipt(
        status: bool,
        logs: Vec<alloy::rpc::types::eth::Log>,
    ) -> alloy::rpc::types::eth::TransactionReceipt {
        alloy::rpc::types::eth::TransactionReceipt {
            inner: envelope(status, logs),
            transaction_hash: Default::default(),
            transaction_index: None,
            block_hash: None,
            block_number: None,
            gas_used: 0,
            effective_gas_price: 0,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        }
    }

    /// The exact log the contract emits: indexed token/quote topics plus the
    /// ABI-encoded (amount, profit) data.
    fn arb_executed_log(
        token: Address,
        quote: Address,
        amount: U256,
        profit: U256,
    ) -> alloy::rpc::types::eth::Log {
        let event = IFlashArbitrage::ArbExecuted {
            token,
            quote,
            amount,
            profit,
        };
        alloy::rpc::types::eth::Log {
            inner: alloy::primitives::Log {
                address: CONTRACT,
                data: event.encode_log_data(),
            },
            block_hash: None,
            block_number: None,
            ..Default::default()
        }
    }

    const CONTRACT: Address = address!("9999999999999999999999999999999999999999");
    const LOAN: Address = address!("1111111111111111111111111111111111111111");
    const QUOTE: Address = address!("2222222222222222222222222222222222222222");

    #[test]
    fn verdict_confirms_on_matching_arb_executed() {
        let r = receipt(
            true,
            vec![arb_executed_log(
                LOAN,
                QUOTE,
                U256::from(1234),
                U256::from(56),
            )],
        );
        assert_eq!(
            verdict_from_receipt(&r, CONTRACT, LOAN),
            ReceiptVerdict::Confirmed {
                token: LOAN,
                amount: U256::from(1234),
                profit: U256::from(56)
            }
        );
    }

    /// A reverted tx is a revert even if it somehow carried the event.
    #[test]
    fn verdict_reports_revert_on_failed_status() {
        let r = receipt(
            false,
            vec![arb_executed_log(LOAN, QUOTE, U256::from(1), U256::from(1))],
        );
        assert_eq!(
            verdict_from_receipt(&r, CONTRACT, LOAN),
            ReceiptVerdict::Reverted
        );
    }

    /// Status success with no logs at all — the silent no-op case.
    #[test]
    fn verdict_reports_missing_event_on_logless_success() {
        let r = receipt(true, vec![]);
        assert_eq!(
            verdict_from_receipt(&r, CONTRACT, LOAN),
            ReceiptVerdict::MissingEvent
        );
    }

    /// A log with the right shape but emitted by another contract must not
    /// confirm our trade.
    #[test]
    fn verdict_ignores_event_from_other_emitter() {
        let mut log = arb_executed_log(LOAN, QUOTE, U256::from(1), U256::from(1));
        log.inner.address = address!("8888888888888888888888888888888888888888");
        let r = receipt(true, vec![log]);
        assert_eq!(
            verdict_from_receipt(&r, CONTRACT, LOAN),
            ReceiptVerdict::MissingEvent
        );
    }

    /// An `ArbExecuted` for a different loan token must not confirm ours.
    #[test]
    fn verdict_ignores_event_for_other_token() {
        let r = receipt(
            true,
            vec![arb_executed_log(QUOTE, LOAN, U256::from(1), U256::from(1))],
        );
        assert_eq!(
            verdict_from_receipt(&r, CONTRACT, LOAN),
            ReceiptVerdict::MissingEvent
        );
    }

    /// A fee-free cycle (profit 0) is still a confirmed trade: the event, not
    /// a positive profit, is what proves the cycle ran.
    #[test]
    fn verdict_confirms_zero_profit_cycle() {
        let r = receipt(
            true,
            vec![arb_executed_log(
                LOAN,
                QUOTE,
                U256::from(1_000_000),
                U256::from(0),
            )],
        );
        assert_eq!(
            verdict_from_receipt(&r, CONTRACT, LOAN),
            ReceiptVerdict::Confirmed {
                token: LOAN,
                amount: U256::from(1_000_000),
                profit: U256::ZERO
            }
        );
    }

    /// The mixed case: an unrelated log ahead of the real one must not stop
    /// the scan.
    #[test]
    fn verdict_scans_past_unrelated_logs() {
        use alloy::primitives::LogData;
        let unrelated = alloy::rpc::types::eth::Log {
            inner: alloy::primitives::Log {
                address: CONTRACT,
                data: LogData::new_unchecked(vec![B256::ZERO], Default::default()),
            },
            block_hash: None,
            block_number: None,
            ..Default::default()
        };
        let r = receipt(
            true,
            vec![
                unrelated,
                arb_executed_log(LOAN, QUOTE, U256::from(7), U256::from(3)),
            ],
        );
        assert_eq!(
            verdict_from_receipt(&r, CONTRACT, LOAN),
            ReceiptVerdict::Confirmed {
                token: LOAN,
                amount: U256::from(7),
                profit: U256::from(3)
            }
        );
    }

    /// The missing-event message must be actionable, not a generic failure.
    #[test]
    fn missing_event_verdict_message_is_actionable() {
        let msg = ReceiptVerdict::MissingEvent.to_string();
        assert!(msg.contains("ArbExecuted"));
        assert!(msg.contains("callback never ran"));
    }

    #[test]
    fn ownership_mismatch_report_is_explicit() {
        use crate::executor::OwnershipMismatch;
        let err = OwnershipMismatch {
            owner: address!("1111111111111111111111111111111111111111"),
            signer: address!("2222222222222222222222222222222222222222"),
        };
        let report = eyre::Report::new(err.clone());
        let msg = format!("{report:#}");
        assert!(msg.contains("0x1111"));
        assert!(msg.contains("0x2222"));
        assert!(msg.contains("PRIVATE_KEY"));
        assert!(msg.contains("restart"));
        let _ = err; // still usable after conversion
    }
}
