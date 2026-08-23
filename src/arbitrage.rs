use crate::dex::get_amount_out;
use alloy::primitives::U256;

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

/// Precomputed swap quotes for one venue over one scan. Both legs carry
/// *real* executable outputs: exact constant-product math for V2/Aero
/// venues, QuoterV2 results (tick/liquidity traversal done on-chain) for
/// V3 venues. None means the venue could not quote that amount (reverted
/// quoter call, empty reserves), and the pair is skipped rather than
/// mispriced.
pub struct VenueQuotes {
    pub venue: usize,
    /// Leg 1 (loan -> quote) output per configured loan size, aligned with
    /// the `loan_amounts` slice passed to `find_opportunity`.
    pub leg1: Vec<Option<U256>>,
    /// Leg 2 (quote -> loan) outputs: `(quote_in, loan_out)` for every
    /// distinct leg-1 output produced by the OTHER venues this scan.
    pub leg2: Vec<(U256, Option<U256>)>,
}

/// Simulate the cycle over every ordered venue pair (i, j) and every loan
/// size, returning the most profitable one clearing `min_profit`, if any.
pub fn find_opportunity(
    loan_amounts: &[U256],
    venues: &[VenueQuotes],
    min_profit: U256,
) -> Option<Opportunity> {
    let mut best: Option<Opportunity> = None;
    for first in venues {
        for (i, &loan_amount) in loan_amounts.iter().enumerate() {
            let Some(quote_out) = first.leg1.get(i).copied().flatten() else {
                continue;
            };
            for second in venues {
                if first.venue == second.venue {
                    continue;
                };
                let Some(amount_out) = second
                    .leg2
                    .iter()
                    .find(|(q, _)| *q == quote_out)
                    .and_then(|(_, out)| *out)
                else {
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
                    quote_out,
                    amount_out,
                    profit,
                };
                if best.is_none_or(|b| opp.profit > b.profit) {
                    best = Some(opp);
                }
            }
        }
    }
    best
}

/// Build the `VenueQuotes` for a V2/Aero venue: exact constant-product math
/// for both legs, given the pool reserves oriented loan-token first and the
/// distinct leg-1 outputs of the other venues.
pub fn v2_quotes(
    venue: usize,
    reserves: crate::dex::PoolReserves,
    fee_bps: u64,
    loan_amounts: &[U256],
    leg2_inputs: &[U256],
) -> VenueQuotes {
    let leg1 = loan_amounts
        .iter()
        .map(|&size| get_amount_out(size, reserves.reserve_in, reserves.reserve_out, fee_bps))
        .collect();
    let leg2 = leg2_inputs
        .iter()
        .map(|&q| {
            (
                q,
                get_amount_out(q, reserves.reserve_out, reserves.reserve_in, fee_bps),
            )
        })
        .collect();
    VenueQuotes { venue, leg1, leg2 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::PoolReserves;

    /// Build a V2-style venue with 1:1-ish reserves, quoting against the
    /// leg-1 outputs of `others` (test helper, indices are venue ids).
    fn v2_venue(
        venue: usize,
        loan: u128,
        quote: u128,
        sizes: &[u128],
        leg2_inputs: &[u128],
        fee_bps: u64,
    ) -> VenueQuotes {
        v2_quotes(
            venue,
            PoolReserves {
                reserve_in: U256::from(loan),
                reserve_out: U256::from(quote),
            },
            fee_bps,
            &sizes.iter().map(|&s| U256::from(s)).collect::<Vec<_>>(),
            &leg2_inputs
                .iter()
                .map(|&s| U256::from(s))
                .collect::<Vec<_>>(),
        )
    }

    /// Build quotes for a set of constant-product venues, wiring each
    /// venue's leg-2 inputs to the other venues' leg-1 outputs (mirrors
    /// what the scanner does with RPC quotes).
    fn pool_set(specs: &[(u128, u128)], sizes: &[u128]) -> Vec<VenueQuotes> {
        let sizes_u: Vec<U256> = sizes.iter().map(|&s| U256::from(s)).collect();
        specs
            .iter()
            .enumerate()
            .map(|(i, &(loan, quote))| {
                let reserves = PoolReserves {
                    reserve_in: U256::from(loan),
                    reserve_out: U256::from(quote),
                };
                let leg2_inputs: Vec<U256> = specs
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .flat_map(|(_, &(l2, q2))| {
                        sizes_u.iter().filter_map(move |&s| {
                            get_amount_out(s, U256::from(l2), U256::from(q2), 30)
                        })
                    })
                    .collect();
                v2_quotes(i, reserves, 30, &sizes_u, &leg2_inputs)
            })
            .collect()
    }

    fn sizes(s: &[u128]) -> Vec<U256> {
        s.iter().map(|&x| U256::from(x)).collect()
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
        assert!(
            low_fee > high_fee,
            "a 0.05% pool must out-yield a 0.3% pool"
        );
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
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_000_000, 1_000_000)], &[10_000]);
        let opp = find_opportunity(&sizes(&[10_000]), &venues, U256::ZERO);
        assert!(opp.is_none(), "identical pools must not be profitable");
    }

    #[test]
    fn price_dislocation_yields_profit_in_one_direction() {
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_100_000, 900_000)], &[10_000]);
        let opp = find_opportunity(&sizes(&[10_000]), &venues, U256::ZERO)
            .expect("dislocated pools should yield an opportunity");
        assert_eq!(opp.first, 0);
        assert_eq!(opp.second, 1);
        assert!(opp.profit > U256::ZERO);
        assert_eq!(opp.amount_out, opp.loan_amount + opp.profit);
    }

    #[test]
    fn mirror_direction_is_found_when_pools_are_swapped() {
        let venues = pool_set(&[(1_100_000, 900_000), (1_000_000, 1_000_000)], &[10_000]);
        let opp = find_opportunity(&sizes(&[10_000]), &venues, U256::ZERO)
            .expect("swapped pools should still yield an opportunity");
        assert_eq!(opp.first, 1);
        assert_eq!(opp.second, 0);
    }

    #[test]
    fn four_venues_route_through_the_dislocated_pool() {
        // Four venues; venue 1 is the only dislocated pool, so any profitable
        // cycle must route its return leg through venue 1.
        let venues = pool_set(
            &[
                (1_000_000, 1_000_000),
                (1_100_000, 900_000),
                (1_000_000, 1_000_000),
                (1_000_000, 1_000_000),
            ],
            &[10_000],
        );
        let opp = find_opportunity(&sizes(&[10_000]), &venues, U256::ZERO)
            .expect("four venues should still find a pair");
        assert!(opp.profit > U256::ZERO);
        assert!(opp.first == 1 || opp.second == 1);
    }

    #[test]
    fn unquotable_venue_skips_only_its_pairs() {
        // Venue 1 cannot quote anything (e.g. QuoterV2 reverted); venues 0
        // and 2 are dislocated, so an opportunity must still be found.
        let mut venues = pool_set(
            &[
                (1_000_000, 1_000_000),
                (1_100_000, 900_000),
                (1_100_000, 900_000),
            ],
            &[10_000],
        );
        venues[1].leg1 = vec![None];
        venues[1].leg2 = venues[1].leg2.iter().map(|&(q, _)| (q, None)).collect();
        let opp = find_opportunity(&sizes(&[10_000]), &venues, U256::ZERO)
            .expect("one dead venue must not abort the search");
        assert!(opp.profit > U256::ZERO);
        assert!(opp.first != 1 && opp.second != 1);
    }

    #[test]
    fn min_profit_threshold_filters_small_gains() {
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_001_000, 999_000)], &[10_000]);
        let opp = find_opportunity(&sizes(&[10_000]), &venues, U256::from(1_000_000u64));
        assert!(
            opp.is_none(),
            "tiny dislocation must fail a large min_profit"
        );
    }

    #[test]
    fn best_of_multiple_sizes_is_selected() {
        let sizes_in = [1_000u128, 10_000, 100_000];
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_100_000, 900_000)], &sizes_in);
        let best = find_opportunity(&sizes(&sizes_in), &venues, U256::ZERO)
            .expect("some size should be profitable");
        assert!(best.profit > U256::ZERO);
        // The optimum is not necessarily the largest size: price impact
        // grows with size, so the search must consider all of them. Each
        // single-size run needs quotes built for that size alone (leg
        // outputs are aligned with the sizes they were quoted for).
        for &s in &sizes_in {
            let single = pool_set(&[(1_000_000, 1_000_000), (1_100_000, 900_000)], &[s]);
            let only = find_opportunity(&sizes(&[s]), &single, U256::ZERO).unwrap();
            assert!(best.profit >= only.profit);
        }
    }

    #[test]
    fn v2_quotes_wire_both_legs() {
        let q = v2_venue(0, 1_000_000, 1_000_000, &[1_000], &[500], 30);
        assert_eq!(q.leg1.len(), 1);
        assert!(q.leg1[0].is_some());
        assert_eq!(q.leg2.len(), 1);
        assert_eq!(q.leg2[0].0, U256::from(500u64));
        assert!(q.leg2[0].1.is_some());
    }
}
