use crate::arbitrage::{Direction, Opportunity};
use crate::config::Config;
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, TxHash};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use eyre::Result;

sol! {
    struct ArbParams {
        address token;
        uint256 amount;
        address routerA;
        address routerB;
        address[] pathA;
        address[] pathB;
        uint256 minProfit;
    }

    #[sol(rpc)]
    interface IFlashArbitrage {
        function execute(ArbParams params) external;
    }
}

/// Map a simulated direction to the on-chain router ordering.
/// Router ordering is resolved against the configured pair router addresses.
pub fn build_params(
    cfg: &Config,
    opp: &Opportunity,
    router_a: Address,
    router_b: Address,
) -> ArbParams {
    let (router_first, router_second) = match opp.direction {
        Direction::ASellBSell => (router_a, router_b),
        Direction::BSellASell => (router_b, router_a),
    };
    ArbParams {
        token: cfg.loan_token,
        amount: opp.loan_amount,
        routerA: router_first,
        routerB: router_second,
        pathA: vec![cfg.loan_token, cfg.quote_token],
        pathB: vec![cfg.quote_token, cfg.loan_token],
        minProfit: cfg.min_profit,
    }
}

/// Simulate `execute` via eth_call without broadcasting.
pub async fn simulate<P: Provider>(provider: &P, contract: Address, params: ArbParams) -> Result<()> {
    let arb = IFlashArbitrage::new(contract, provider);
    arb.execute(params).call().await?;
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
