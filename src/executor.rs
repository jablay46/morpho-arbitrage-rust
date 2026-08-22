use crate::arbitrage::Opportunity;
use crate::config::{Config, Venue};
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use eyre::Result;

sol! {
    struct SwapLeg {
        address router;
        uint8 kind;
        address factory;
        bool stable;
        uint256 minOut;
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

fn build_leg(venue: &Venue, min_out: U256) -> SwapLeg {
    SwapLeg {
        router: venue.router,
        kind: venue.kind as u8,
        factory: venue.factory,
        stable: venue.stable,
        minOut: min_out,
    }
}

/// Scale a simulated output down by the slippage tolerance.
fn with_slippage(expected: U256, slippage_bps: u64) -> U256 {
    expected * U256::from(10_000u64 - slippage_bps) / U256::from(10_000u64)
}

/// Resolve the chosen venue pair to its swap legs and build the calldata.
pub fn build_params(cfg: &Config, opp: &Opportunity) -> ArbParams {
    ArbParams {
        token: cfg.loan_token,
        quote: cfg.quote_token,
        amount: opp.loan_amount,
        legA: build_leg(
            &cfg.venues[opp.first],
            with_slippage(opp.quote_out, cfg.slippage_bps),
        ),
        legB: build_leg(
            &cfg.venues[opp.second],
            with_slippage(opp.amount_out, cfg.slippage_bps),
        ),
        minProfit: cfg.min_profit,
    }
}

/// Simulate `execute` via eth_call without broadcasting. The call must carry
/// `from` = the contract owner, otherwise the contract's `onlyOwner` guard
/// reverts the simulation. The owner is read on-chain so simulation works in
/// dry-run mode without a valid private key.
pub async fn simulate<P: Provider>(
    provider: &P,
    contract: Address,
    params: ArbParams,
) -> Result<()> {
    let arb = IFlashArbitrage::new(contract, provider);
    let owner = arb.owner().call().await?;
    arb.execute(params).from(owner).call().await?;
    Ok(())
}

/// Broadcast `execute` and wait for the receipt.
pub async fn execute(
    rpc_url: &str,
    private_key: &str,
    contract: Address,
    params: ArbParams,
) -> Result<TxHash> {
    let signer: PrivateKeySigner = private_key.parse()?;
    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse()?);
    let arb = IFlashArbitrage::new(contract, &provider);
    let pending = arb.execute(params).send().await?;
    let receipt = pending.get_receipt().await?;
    Ok(receipt.transaction_hash)
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
        "f2121286",
        "0000000000000000000000001111111111111111111111111111111111111111",
        "0000000000000000000000002222222222222222222222222222222222222222",
        "0000000000000000000000000000000000000000000000000de0b6b3a7640000",
        "0000000000000000000000003333333333333333333333333333333333333333",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000384",
        "0000000000000000000000004444444444444444444444444444444444444444",
        "0000000000000000000000000000000000000000000000000000000000000001",
        "0000000000000000000000005555555555555555555555555555555555555555",
        "0000000000000000000000000000000000000000000000000000000000000001",
        "00000000000000000000000000000000000000000000000000000000000003e9",
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
        assert_eq!(params.token, address!("1111111111111111111111111111111111111111"));
        assert_eq!(params.quote, address!("2222222222222222222222222222222222222222"));
        assert_eq!(params.amount, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(params.legA.router, address!("3333333333333333333333333333333333333333"));
        assert_eq!(params.legA.kind, 0);
        assert_eq!(params.legA.factory, Address::ZERO);
        assert!(!params.legA.stable);
        assert_eq!(params.legA.minOut, U256::from(900u64));
        assert_eq!(params.legB.router, address!("4444444444444444444444444444444444444444"));
        assert_eq!(params.legB.kind, 1);
        assert_eq!(params.legB.factory, address!("5555555555555555555555555555555555555555"));
        assert!(params.legB.stable);
        assert_eq!(params.legB.minOut, U256::from(1_001u64));
        assert_eq!(params.minProfit, U256::from(12_345u64));

        assert_eq!(
            params.abi_encode(),
            payload,
            "re-encoding must reproduce the exact Solidity bytes"
        );
    }
}

