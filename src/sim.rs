//! Local transaction simulation with an embedded EVM (revm).
//!
//! Replaces `eth_estimateGas`/`eth_call` round-trips on the opportunity path
//! with an in-process execution against chain state fetched lazily through
//! the normal RPC provider ([`AlloyDB`]). Each simulation pins the same
//! block id the scan used, so results are semantics-identical to the node's
//! own estimate; account code and storage are fetched on demand and dropped
//! after the call (a fresh instance per simulation keeps the state honest —
//! caching across blocks would serve stale storage).
//!
//! Failure modes are split deliberately: a contract revert is a per-
//! opportunity verdict ([`SimOutcome::Reverted`], same as an
//! `eth_estimateGas` revert), while transport/DB errors surface as `Err` so
//! the caller can fall back to the node's `eth_estimateGas`.

use alloy::primitives::{Address, Bytes, TxKind, U256};
use alloy::providers::Provider;
use eyre::{eyre, Result};
use revm::context::TxEnv;
use revm::database::{AlloyDB, BlockId, CacheDB};
use revm::database_interface::WrapDatabaseAsync;
use revm::{Context, ExecuteEvm, MainBuilder, MainContext};

/// Gas ceiling handed to the EVM for the simulated call. Deliberately far
/// above any realistic flashloan-arb execution so the estimate is bounded by
/// actual consumption, not the cap (revm, like eth_estimateGas, returns gas
/// *used*, which is independent of the limit as long as it suffices).
const SIM_GAS_LIMIT: u64 = 10_000_000;

/// Verdict of a local simulation.
pub enum SimOutcome {
    /// Call executed successfully; carries gas used and the return data.
    Success { gas_used: u64, output: alloy::primitives::Bytes },
    /// Call reverted; carries the decoded revert reason / raw output.
    Reverted(String),
}

/// Execute `calldata` against `contract` from `owner` in a local revm
/// instance, with chain state lazily fetched from `provider` at `block`.
///
/// This is a synchronous, blocking call (revm is not async); the DB wrapper
/// drives the provider's async fetches on the ambient multi-threaded tokio
/// runtime. Callers inside async code get the same semantics as an RPC call.
pub fn simulate_call<P: Provider>(
    provider: P,
    contract: Address,
    owner: Address,
    calldata: Bytes,
    block: BlockId,
) -> Result<SimOutcome> {
    let alloy_db = AlloyDB::new(provider, block);
    let async_db = WrapDatabaseAsync::new(alloy_db)
        .ok_or_else(|| eyre!("local sim requires a multi-threaded tokio runtime"))?;
    // AlloyDB only implements DatabaseAsyncRef; CacheDB lifts any DatabaseRef
    // into Database and de-duplicates repeated account/storage lookups within
    // this one execution.
    let db = CacheDB::new(async_db);

    let ctx = Context::mainnet()
        .modify_cfg_chained(|cfg| {
            // Simulation must mirror eth_estimateGas semantics: the call is
            // unsigned, the caller may not pay for gas, and the nonce is not
            // checked by the node either.
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
        })
        .with_db(db);
    let mut evm = ctx.build_mainnet();

    let tx = TxEnv::builder()
        .caller(owner)
        .kind(TxKind::Call(contract))
        .data(calldata)
        .value(U256::ZERO)
        .gas_limit(SIM_GAS_LIMIT)
        .gas_price(0)
        .build()
        .map_err(|e| eyre!("invalid sim tx: {e}"))?;

    let out = evm
        .transact(tx)
        .map_err(|e| eyre!("local sim execution error: {e}"))?;
    let result = out.result;
    if result.is_success() {
        Ok(SimOutcome::Success {
            gas_used: result.tx_gas_used(),
            output: result.output().cloned().unwrap_or_default(),
        })
    } else {
        let reason = result
            .output()
            .map(|b| decode_revert_reason(b))
            .unwrap_or_else(|| format!("halted: {result:?}"));
        Ok(SimOutcome::Reverted(reason))
    }
}

/// Best-effort decode of a revert payload: strips the Error(string) selector
/// when present, else renders the raw bytes.
fn decode_revert_reason(data: &[u8]) -> String {
    const ERROR_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
    if data.len() >= 4 + 32 + 32 && data[..4] == ERROR_SELECTOR {
        let str_len = U256::from_be_slice(&data[36..68]).to::<usize>();
        let start = 68;
        let end = (start + str_len).min(data.len());
        if let Ok(s) = std::str::from_utf8(&data[start..end]) {
            return format!("reverted: {s}");
        }
    }
    format!("reverted: 0x{}", alloy::hex::encode(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_error_string_revert() {
        // Error("too thin") payload built by hand.
        let reason = b"too thin";
        let mut data = Vec::new();
        data.extend_from_slice(&[0x08, 0xc3, 0x79, 0xa0]);
        data.extend_from_slice(&[0u8; 32]);
        data[31] = 32; // offset
        data.extend_from_slice(&[0u8; 32]);
        data[63] = reason.len() as u8;
        data.extend_from_slice(reason);
        data.resize(68 + 32, 0); // padded
        let s = decode_revert_reason(&data[..68 + reason.len()]);
        assert_eq!(s, "reverted: too thin");
    }

    #[test]
    fn raw_payload_falls_back_to_hex() {
        let s = decode_revert_reason(&[0xde, 0xad]);
        assert_eq!(s, "reverted: 0xdead");
    }
}
