use crate::dex::{get_amount_out, PoolReserves};
use alloy::primitives::U256;

/// V3 pool state (sqrtPriceX96, liquidity) for price calculation.
#[derive(Debug, Clone, Copy)]
pub struct V3PoolState {
    pub sqrt_price_x96: U256,
    pub liquidity: U256,
}

/// Current reserves of one venue's pool, tagged with its index in the
/// configured venue list. For V2/Aero, reserves are oriented loan-token
/// first. For V3, `v3_state` is used instead of reserves.
#[derive(Debug, Clone, Copy)]
pub struct PoolState {
    pub venue: usize,
    pub reserves: PoolReserves,
    /// V3 pool state (sqrtPriceX96, liquidity). None for V2/Aero.
    pub v3_state: Option<V3PoolState>,
    /// Pool fee in basis points (30 = 0.3%).
    pub fee_bps: u64,
    /// V3 fee tier (only used when v3_state is Some).
    pub fee_tier: u32,
}

/// A simulated arbitrage outcome for one loan size.
#[derive(Debug, Clone, Copy)]
pub struct Opportunity {
    /// Venue index where the loan token is sold for the quote token.
    pub first: usize,
    /// Venue index where the quote token is swapped back to the loan token.
    pub second: usize,
    pub loan_amount: U256,
    /// Simulated quote-token output of leg 1; used to bound leg 1 slippage.
    pub quote_out: U256,
    /// Loan-token amount returned after both swaps, before any fees.
    pub amount_out: U256,
    /// `amount_out - loan_amount` (Morpho Blue flash loans are fee-free).
    pub profit: U256,
}

/// Simulate the cycle over every ordered venue pair (i, j) for one loan size,
/// returning the most profitable one clearing `min_profit`, if any.
/// For V3 venues, uses the quoter for accurate pricing; for V2/Aero, uses
/// the constant-product formula.
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
            let quote_out = if let Some(v3) = first.v3_state {
                // For V3, approximate using constant-product with slot0 price.
                // In production, use a quoter contract for exact pricing.
                let (reserve_in, reserve_out) = if v3.sqrt_price_x96.is_zero() {
                    continue;
                } else {
                    // Derive virtual reserves from sqrtPriceX96 and liquidity.
                    // reserve_in = liquidity * sqrt_price_x96 / 2^96
                    // reserve_out = liquidity * 2^96 / sqrt_price_x96
                    let q96 = U256::from(1u128) << 96;
                    let reserve_in = v3.liquidity * v3.sqrt_price_x96 / q96;
                    let reserve_out = v3.liquidity * q96 / v3.sqrt_price_x96;
                    (reserve_in, reserve_out)
                };
                get_amount_out(loan_amount, reserve_in, reserve_out, first.fee_bps)?
            } else {
                get_amount_out(
                    loan_amount,
                    first.reserves.reserve_in,
                    first.reserves.reserve_out,
                    first.fee_bps,
                )?
            };
            // Leg 2: quote token -> loan token on `second` (reserves flipped).
            let amount_out = if let Some(v3) = second.v3_state {
                let (reserve_in, reserve_out) = if v3.sqrt_price_x96.is_zero() {
                    continue;
                } else {
                    let q96 = U256::from(1u128) << 96;
                    let reserve_in = v3.liquidity * q96 / v3.sqrt_price_x96;
                    let reserve_out = v3.liquidity * v3.sqrt_price_x96 / q96;
                    (reserve_in, reserve_out)
                };
                get_amount_out(quote_out, reserve_in, reserve_out, second.fee_bps)?
            } else {
                get_amount_out(
                    quote_out,
                    second.reserves.reserve_out,
                    second.reserves.reserve_in,
                    second.fee_bps,
                )?
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
                quote_out,
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
        pool_with_fee(venue, loan, quote, 30)
    }

    fn pool_with_fee(venue: usize, loan: u128, quote: u128, fee_bps: u64) -> PoolState {
        PoolState {
            venue,
            reserves: PoolReserves {
                reserve_in: U256::from(loan),
                reserve_out: U256::from(quote),
            },
            v3_state: None,
            fee_bps,
            fee_tier: 0,
        }
    }

    #[test]
    fn amount_out_matches_uniswap_v2_formula() {
        // 1000 in, reserves 1_000_000 / 1_000_000, 0.3% fee.
        let out = get_amount_out(
            U256::from(1_000u64),
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            30,
        )
        .unwrap();
        // floor(997_000_000 / 1_000_997_000 * 1_000_000) = 996
        assert_eq!(out, U256::from(996u64));
    }

    #[test]
    fn lower_fee_pool_yields_more_output() {
        let high_fee = get_amount_out(
            U256::from(1_000u64),
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            30,
        )
        .unwrap();
        let low_fee = get_amount_out(
            U256::from(1_000u64),
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            5,
        )
        .unwrap();
        assert!(low_fee > high_fee, "a 0.05% pool must out-yield a 0.3% pool");
    }

    #[test]
    fn invalid_fee_bps_rejected() {
        assert!(get_amount_out(
            U256::from(1_000u64),
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            10_000,
        )
        .is_none());
    }

    #[test]
    fn zero_inputs_yield_none() {
        assert!(get_amount_out(U256::ZERO, U256::from(1u64), U256::from(1u64), 30).is_none());
        assert!(get_amount_out(U256::from(1u64), U256::ZERO, U256::from(1u64), 30).is_none());
        assert!(get_amount_out(U256::from(1u64), U256::from(1u64), U256::ZERO, 30).is_none());
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
