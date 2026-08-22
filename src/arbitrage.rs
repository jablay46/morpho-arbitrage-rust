use crate::dex::{get_amount_out, PoolReserves};
use alloy::primitives::U256;

/// Current reserves of one venue's pool, tagged with its index in the
/// configured venue list. Reserves are oriented loan-token first.
#[derive(Debug, Clone, Copy)]
pub struct PoolState {
    pub venue: usize,
    pub reserves: PoolReserves,
}

/// A simulated arbitrage outcome for one loan size.
#[derive(Debug, Clone, Copy)]
pub struct Opportunity {
    /// Venue index where the loan token is sold for the quote token.
    pub first: usize,
    /// Venue index where the quote token is swapped back to the loan token.
    pub second: usize,
    pub loan_amount: U256,
    /// Loan-token amount returned after both swaps, before any fees.
    pub amount_out: U256,
    /// `amount_out - loan_amount` (Morpho Blue flash loans are fee-free).
    pub profit: U256,
}

/// Simulate the cycle over every ordered venue pair (i, j) for one loan size,
/// returning the most profitable one clearing `min_profit`, if any.
pub fn find_opportunity(
    loan_amount: U256,
    pools: &[PoolState],
    min_profit: U256,
) -> Option<Opportunity> {
    let mut best: Option<Opportunity> = None;
    for first in pools {
        for second in pools {
            if first.venue == second.venue {
                continue;
            }
            // Leg 1: loan token -> quote token on `first`.
            let Some(quote_out) = get_amount_out(
                loan_amount,
                first.reserves.reserve_in,
                first.reserves.reserve_out,
            ) else {
                continue;
            };
            // Leg 2: quote token -> loan token on `second` (reserves flipped).
            let Some(amount_out) = get_amount_out(
                quote_out,
                second.reserves.reserve_out,
                second.reserves.reserve_in,
            ) else {
                continue;
            };
            let Some(profit) = amount_out.checked_sub(loan_amount) else {
                continue;
            };
            if profit.is_zero() || profit < min_profit {
                continue;
            }
            let opp = Opportunity {
                first: first.venue,
                second: second.venue,
                loan_amount,
                amount_out,
                profit,
            };
            if best.is_none_or(|b| opp.profit > b.profit) {
                best = Some(opp);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(venue: usize, loan: u128, quote: u128) -> PoolState {
        PoolState {
            venue,
            reserves: PoolReserves {
                reserve_in: U256::from(loan),
                reserve_out: U256::from(quote),
            },
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
        let pools = vec![pool(0, 1_000_000, 1_000_000), pool(1, 1_000_000, 1_000_000)];
        let opp = find_opportunity(U256::from(10_000u64), &pools, U256::ZERO);
        assert!(opp.is_none(), "identical pools must not be profitable");
    }

    #[test]
    fn price_dislocation_yields_profit_in_one_direction() {
        let pools = vec![
            pool(0, 1_000_000, 1_000_000),
            pool(1, 1_100_000, 900_000),
        ];
        let opp = find_opportunity(U256::from(10_000u64), &pools, U256::ZERO)
            .expect("dislocated pools should yield an opportunity");
        assert_eq!(opp.first, 0);
        assert_eq!(opp.second, 1);
        assert!(opp.profit > U256::ZERO);
        assert_eq!(opp.amount_out, opp.loan_amount + opp.profit);
    }

    #[test]
    fn mirror_direction_is_found_when_pools_are_swapped() {
        let pools = vec![
            pool(0, 1_100_000, 900_000),
            pool(1, 1_000_000, 1_000_000),
        ];
        let opp = find_opportunity(U256::from(10_000u64), &pools, U256::ZERO)
            .expect("swapped pools should still yield an opportunity");
        assert_eq!(opp.first, 1);
        assert_eq!(opp.second, 0);
    }

    #[test]
    fn four_venues_route_through_the_dislocated_pool() {
        // Four venues; venue 1 is the only dislocated pool, so any profitable
        // cycle must route its return leg through venue 1.
        let pools = vec![
            pool(0, 1_000_000, 1_000_000),
            pool(1, 1_100_000, 900_000),
            pool(2, 1_000_000, 1_000_000),
            pool(3, 1_000_000, 1_000_000),
        ];
        let opp = find_opportunity(U256::from(10_000u64), &pools, U256::ZERO)
            .expect("four venues should still find a pair");
        assert!(opp.profit > U256::ZERO);
        assert!(opp.first == 1 || opp.second == 1);
    }

    #[test]
    fn min_profit_threshold_filters_small_gains() {
        let pools = vec![
            pool(0, 1_000_000, 1_000_000),
            pool(1, 1_001_000, 999_000),
        ];
        let opp = find_opportunity(U256::from(10_000u64), &pools, U256::from(1_000_000u64));
        assert!(opp.is_none(), "tiny dislocation must fail a large min_profit");
    }

    #[test]
    fn best_of_multiple_sizes_can_be_selected() {
        let pools = vec![
            pool(0, 1_000_000, 1_000_000),
            pool(1, 1_100_000, 900_000),
        ];
        let sizes = [1_000u64, 10_000, 100_000];
        let best = sizes
            .iter()
            .filter_map(|&s| find_opportunity(U256::from(s), &pools, U256::ZERO))
            .max_by_key(|o| o.profit)
            .unwrap();
        assert!(best.profit > U256::ZERO);
    }
}
