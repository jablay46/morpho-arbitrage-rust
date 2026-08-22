use crate::dex::{get_amount_out, PoolReserves};
use alloy::primitives::U256;

/// Which direction to run the two-hop cycle.
///
/// Start with `amount` of the loan token.
/// - `ASellBSell`: sell loan token on A for quote token, sell quote token on B back to loan token.
/// - `BSellASell`: the mirror image (sell on B first, buy back on A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ASellBSell,
    BSellASell,
}

/// A simulated arbitrage outcome for one loan size.
#[derive(Debug, Clone, Copy)]
pub struct Opportunity {
    pub direction: Direction,
    pub loan_amount: U256,
    /// Loan-token amount returned after both swaps, before any fees.
    pub amount_out: U256,
    /// `amount_out - loan_amount` (Morpho Blue flash loans are fee-free).
    pub profit: U256,
}

/// Simulate both cycle directions for a given loan size and pool states,
/// returning the profitable one with the highest profit, if any.
///
/// `reserves_a` / `reserves_b` are oriented with the loan token as `reserve_in`.
pub fn find_opportunity(
    loan_amount: U256,
    reserves_a: PoolReserves,
    reserves_b: PoolReserves,
    min_profit: U256,
) -> Option<Opportunity> {
    let try_direction = |direction: Direction| -> Option<U256> {
        let (first, second) = match direction {
            Direction::ASellBSell => (reserves_a, reserves_b),
            Direction::BSellASell => (reserves_b, reserves_a),
        };
        // Leg 1: loan token -> quote token on `first`.
        let quote_out = get_amount_out(loan_amount, first.reserve_in, first.reserve_out)?;
        // Leg 2: quote token -> loan token on `second` (reserves swapped).
        get_amount_out(quote_out, second.reserve_out, second.reserve_in)
    };

    let candidates = [Direction::ASellBSell, Direction::BSellASell]
        .into_iter()
        .filter_map(|direction| {
            let amount_out = try_direction(direction)?;
            let profit = amount_out.checked_sub(loan_amount)?;
            (profit >= min_profit && !profit.is_zero()).then_some(Opportunity {
                direction,
                loan_amount,
                amount_out,
                profit,
            })
        });

    candidates.max_by_key(|o| o.profit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(loan: u128, quote: u128) -> PoolReserves {
        PoolReserves {
            reserve_in: U256::from(loan),
            reserve_out: U256::from(quote),
        }
    }

    #[test]
    fn amount_out_matches_uniswap_v2_formula() {
        // 1000 in, reserves 1_000_000 / 1_000_000, 0.3% fee.
        let out = get_amount_out(
            U256::from(1_000u64),
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
        )
        .unwrap();
        // floor(997_000_000 / 1_000_997_000 * 1_000_000) = 996
        assert_eq!(out, U256::from(996u64));
    }

    #[test]
    fn zero_inputs_yield_none() {
        assert!(get_amount_out(U256::ZERO, U256::from(1u64), U256::from(1u64)).is_none());
        assert!(get_amount_out(U256::from(1u64), U256::ZERO, U256::from(1u64)).is_none());
        assert!(get_amount_out(U256::from(1u64), U256::from(1u64), U256::ZERO).is_none());
    }

    #[test]
    fn balanced_pools_have_no_opportunity() {
        let p = pool(1_000_000, 1_000_000);
        let opp = find_opportunity(U256::from(10_000u64), p, p, U256::ZERO);
        assert!(opp.is_none(), "identical pools must not be profitable");
    }

    #[test]
    fn price_dislocation_yields_profit_in_one_direction() {
        // Same nominal pools but quote token priced differently: B has more
        // loan token per quote token, so selling loan on A and buying back on B wins.
        let a = pool(1_000_000, 1_000_000);
        let b = pool(1_100_000, 900_000);
        let opp = find_opportunity(U256::from(10_000u64), a, b, U256::ZERO)
            .expect("dislocated pools should yield an opportunity");
        assert_eq!(opp.direction, Direction::ASellBSell);
        assert!(opp.profit > U256::ZERO);
        assert_eq!(opp.amount_out, opp.loan_amount + opp.profit);
    }

    #[test]
    fn mirror_direction_is_found_when_pools_are_swapped() {
        let a = pool(1_100_000, 900_000);
        let b = pool(1_000_000, 1_000_000);
        let opp = find_opportunity(U256::from(10_000u64), a, b, U256::ZERO)
            .expect("swapped pools should still yield an opportunity");
        assert_eq!(opp.direction, Direction::BSellASell);
    }

    #[test]
    fn min_profit_threshold_filters_small_gains() {
        let a = pool(1_000_000, 1_000_000);
        let b = pool(1_001_000, 999_000);
        let opp = find_opportunity(U256::from(10_000u64), a, b, U256::from(1_000_000u64));
        assert!(opp.is_none(), "tiny dislocation must fail a large min_profit");
    }

    #[test]
    fn best_of_multiple_sizes_can_be_selected() {
        let a = pool(1_000_000, 1_000_000);
        let b = pool(1_100_000, 900_000);
        let sizes = [1_000u64, 10_000, 100_000];
        let best = sizes
            .iter()
            .filter_map(|&s| find_opportunity(U256::from(s), a, b, U256::ZERO))
            .max_by_key(|o| o.profit)
            .unwrap();
        assert!(best.profit > U256::ZERO);
    }
}
