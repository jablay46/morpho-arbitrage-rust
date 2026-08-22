use crate::dex::{get_amount_out, PoolReserves};
use alloy::primitives::U256;

/// V3 pool state (sqrtPriceX96, liquidity) for price calculation.
#[derive(Debug, Clone, Copy)]
pub struct V3PoolState {
    pub sqrt_price_x96: U256,
    pub liquidity: U256,
    /// True when the loan token is the pool's token0; sqrtPriceX96 encodes
    /// token1/token0, so the virtual-reserve derivation flips depending on
    /// which side the loan token sits.
    pub loan_is_token0: bool,
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

/// Widen-multiply then divide without overflowing U256:
/// `a * b / den` computed via U512 intermediates. Returns None when the
/// result does not fit back into U256 (or den is zero).
fn mul_div(a: U256, b: U256, den: U256) -> Option<U256> {
    use alloy::primitives::U512;
    if den.is_zero() {
        return None;
    }
    let product = a.widening_mul::<256, 4, 512, 8>(b);
    let (quotient, _) = product.div_rem(U512::from_limbs_slice(den.as_limbs()));
    // quotient fits U256 only if its high half is zero.
    let limbs = quotient.as_limbs();
    if limbs[4..].iter().any(|&l| l != 0) {
        return None;
    }
    Some(U256::from_limbs_slice(&limbs[..4]))
}

/// True when the pool's spot output can be computed without overflow.
/// Kept for symmetry with the scanner's venue filtering; pools at extreme
/// ticks are skipped rather than mispriced.
pub fn v3_priceable(v3: &V3PoolState) -> bool {
    v3_spot_amount_out(v3, 3000, U256::from(1u64)).is_some()
}

/// Spot output of a V3 pool for `amount_in` of the loan token, derived from
/// sqrtPriceX96 with U512 intermediates: P = (sqrtP/2^96)^2, so
/// amount_out = amount_in * sqrtP^2 / 2^192 (or its inverse, depending on
/// token orientation), less the pool fee. This is a spot-price estimate that
/// ignores slippage within the pool (tick liquidity), so it slightly
/// overestimates output for large trades; the on-chain minOut/profit checks
/// remain the backstop.
pub fn v3_spot_amount_out(v3: &V3PoolState, fee_tier: u32, amount_in: U256) -> Option<U256> {
    use alloy::primitives::U512;
    if v3.sqrt_price_x96.is_zero() || amount_in.is_zero() {
        return None;
    }
    let sqrt512 = U512::from_limbs_slice(v3.sqrt_price_x96.as_limbs());
    let p_sq = sqrt512 * sqrt512; // ~2^320 max, fits U512
    let in512 = U512::from_limbs_slice(amount_in.as_limbs());
    let q192 = U512::from(1u128) << 192;
    let out512: U512 = if v3.loan_is_token0 {
        // price = token1/token0 -> out = in * P
        in512 * p_sq / q192
    } else {
        // out = in / P
        if p_sq.is_zero() {
            return None;
        }
        in512 * q192 / p_sq
    };
    let limbs = out512.as_limbs();
    if limbs[4..].iter().any(|&l| l != 0) {
        return None; // doesn't fit U256
    }
    let out = U256::from_limbs_slice(&limbs[..4]);
    // Apply the pool fee (fee_tier is in hundredths of a bip: 500 = 0.05%).
    Some(out * U256::from(1_000_000u64 - fee_tier as u64) / U256::from(1_000_000u64))
}

/// Simulate the cycle over every ordered venue pair (i, j) for one loan size,
/// returning the most profitable one clearing `min_profit`, if any.
/// For V3 venues, outputs are estimated from the slot0 spot price (see
/// v3_spot_amount_out); for V2/Aero, the exact constant-product formula is
/// used. Unpriceable pairs are skipped individually.
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
                let Some(out) = v3_spot_amount_out(&v3, first.fee_tier, loan_amount) else {
                    continue;
                };
                out
            } else {
                let Some(out) = get_amount_out(
                    loan_amount,
                    first.reserves.reserve_in,
                    first.reserves.reserve_out,
                    first.fee_bps,
                ) else {
                    continue;
                };
                out
            };
            // Leg 2: quote token -> loan token on `second` (reserves flipped).
            let amount_out = if let Some(v3) = second.v3_state {
                let flipped = V3PoolState {
                    loan_is_token0: !v3.loan_is_token0,
                    ..v3
                };
                let Some(out) = v3_spot_amount_out(&flipped, second.fee_tier, quote_out) else {
                    continue;
                };
                out
            } else {
                let Some(out) = get_amount_out(
                    quote_out,
                    second.reserves.reserve_out,
                    second.reserves.reserve_in,
                    second.fee_bps,
                ) else {
                    continue;
                };
                out
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
    fn mul_div_handles_large_intermediates_without_overflow() {
        // liquidity ~2^120, sqrtPriceX96 ~2^100 -> product ~2^220 fits U256.
        let liq = U256::from(1u128) << 120;
        let px = U256::from(1u128) << 100;
        let q96 = U256::from(1u128) << 96;
        let r = mul_div(liq, px, q96).expect("fits");
        assert_eq!(r, U256::from(1u128) << 124);
    }

    #[test]
    fn mul_div_wide_product_exceeding_u256_still_computes() {
        // Realistic V3 magnitudes: liquidity ~2^120, sqrtPriceX96 ~2^157
        // (mid-range price). The raw product ~2^277 overflows U256, but the
        // U512 intermediate keeps it exact and the quotient fits.
        let liq = U256::from(1u128) << 120;
        let px = U256::from(1u128) << 157;
        let q96 = U256::from(1u128) << 96;
        let r = mul_div(liq, px, q96).expect("quotient fits U256");
        assert_eq!(r, U256::from(1u128) << 181);
    }

    #[test]
    fn mul_div_returns_none_on_zero_denominator_or_oversize_quotient() {
        assert!(mul_div(U256::from(1u64), U256::from(1u64), U256::ZERO).is_none());
        // quotient ~2^256 does not fit U256.
        assert!(mul_div(U256::MAX, U256::MAX, U256::from(1u64)).is_none());
    }

    #[test]
    fn unpriceable_pool_skips_only_that_pair() {
        // Venue 1 has zero reserves (unpriceable); venues 0 and 2 are
        // dislocated, so an opportunity must still be found between them.
        let pools = vec![
            pool(0, 1_000_000, 1_000_000),
            pool(1, 0, 0),
            pool(2, 1_100_000, 900_000),
        ];
        let opp = find_opportunity(U256::from(10_000u64), &pools, U256::ZERO)
            .expect("one dead pool must not abort the search");
        assert!(opp.profit > U256::ZERO);
        assert!(opp.first != 1 && opp.second != 1);
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
