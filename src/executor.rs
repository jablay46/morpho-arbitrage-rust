use crate::arbitrage::Opportunity;
use crate::config::{Config, Venue};
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, TxHash};
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
    }
}

fn build_leg(venue: &Venue) -> SwapLeg {
    SwapLeg {
        router: venue.router,
        kind: venue.kind as u8,
        factory: venue.factory,
        stable: venue.stable,
    }
}

/// Resolve the chosen venue pair to its swap legs and build the calldata.
pub fn build_params(cfg: &Config, opp: &Opportunity) -> ArbParams {
    ArbParams {
        token: cfg.loan_token,
        quote: cfg.quote_token,
        amount: opp.loan_amount,
        legA: build_leg(&cfg.venues[opp.first]),
        legB: build_leg(&cfg.venues[opp.second]),
        minProfit: cfg.min_profit,
    }
}

/// Simulate `execute` via eth_call without broadcasting. The call must carry
/// `from` = the contract owner, otherwise the contract's `onlyOwner` guard
/// reverts the simulation.
pub async fn simulate<P: Provider>(
    provider: &P,
    contract: Address,
    params: ArbParams,
    from: Address,
) -> Result<()> {
    let arb = IFlashArbitrage::new(contract, provider);
    arb.execute(params).from(from).call().await?;
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
