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
    /// True when leg 1 was priced by local CL math (vs Quoter). Only then
    /// does the execution path re-validate against the on-chain Quoter — a
    /// Quoter-sourced output needs no validation against itself.
    pub leg1_local: bool,
    /// True when leg 2 was priced by local CL math. Same rationale.
    pub leg2_local: bool,
}

/// A leg output together with its provenance: `true` = priced by local CL
/// math (so the execution path re-validates against the on-chain Quoter),
/// `false` = Quoter/RPC-sourced or V2 reserves math (no persistent local
/// state to validate).
pub type LegOutput = (Option<U256>, bool);

/// Precomputed swap quotes for one venue over one scan. Both legs carry
/// *real* executable outputs: exact constant-product math for V2/Aero
/// venues, QuoterV2 results (tick/liquidity traversal done on-chain) for
/// V3 venues. None means the venue could not quote that amount (reverted
/// quoter call, empty reserves), and the pair is skipped rather than
/// mispriced.
pub struct VenueQuotes {
    pub venue: usize,
    /// Leg 1 (loan -> quote) output per configured loan size, aligned with
    /// the `loan_amounts` slice passed to `ranked_opportunities`.
    pub leg1: Vec<LegOutput>,
    /// Leg 2 (quote -> loan) outputs: `(quote_in, (loan_out, is_local))` for
    /// every distinct leg-1 output produced by the OTHER venues this scan.
    pub leg2: Vec<(U256, LegOutput)>,
}

/// Simulate the cycle over every ordered venue pair (i, j) and every loan
/// size, returning all profitable candidates sorted by gross profit
/// descending. Ties are not considered: equal-profit candidates have
/// identical net margins, so any order among them is fine (sort is on the
/// profit key only).
pub fn ranked_opportunities(
    loan_amounts: &[U256],
    venues: &[VenueQuotes],
    min_profit: U256,
) -> Vec<Opportunity> {
    let mut out = Vec::new();
    for first in venues {
        for (i, &loan_amount) in loan_amounts.iter().enumerate() {
            let Some((quote_out, leg1_local)) = first.leg1.get(i).copied() else {
                continue;
            };
            let Some(quote_out) = quote_out else { continue };
            for second in venues {
                if first.venue == second.venue {
                    continue;
                };
                let Some((amount_out, leg2_local)) = second
                    .leg2
                    .iter()
                    .find(|(q, _)| *q == quote_out)
                    .map(|(_, out)| *out)
                else {
                    continue;
                };
                let Some(amount_out) = amount_out else {
                    continue;
                };
                let Some(profit) = amount_out.checked_sub(loan_amount) else {
                    continue;
                };
                if profit.is_zero() || profit < min_profit {
                    continue;
                }
                out.push(Opportunity {
                    first: first.venue,
                    second: second.venue,
                    loan_amount,
                    quote_out,
                    amount_out,
                    profit,
                    leg1_local,
                    leg2_local,
                });
            }
        }
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.profit));
    out
}

/// Outcome of one candidate's gas evaluation, fed to `pick_best_net`.
pub enum GasOutcome {
    /// Gas simulation succeeded; cost in loan-token units.
    Priced(U256),
    /// Gas simulation reverted or the candidate is otherwise unexecutable.
    Rejected,
}

/// Pick the highest net-profit (gross − gas) candidate clearing `min_profit`
/// from pre-evaluated results. Pure and testable: the async gas simulations
/// live in the caller; this only compares numbers. `results` are expected in
/// gross-descending order (as produced by `ranked_opportunities`), though the
/// function is correct for any order.
pub fn pick_best_net(
    results: impl IntoIterator<Item = (Opportunity, GasOutcome)>,
    min_profit: U256,
) -> Option<(Opportunity, U256)> {
    let mut best: Option<(Opportunity, U256)> = None;
    for (opp, outcome) in results {
        let GasOutcome::Priced(gas) = outcome else {
            continue;
        };
        let net = opp.profit.saturating_sub(gas);
        if net < min_profit {
            continue;
        }
        if best.as_ref().is_none_or(|(incumbent, incumbent_gas)| {
            net > incumbent.profit.saturating_sub(*incumbent_gas)
        }) {
            best = Some((opp, gas));
        }
    }
    best
}

/// True when `candidate_gross` can still beat the incumbent's net even at
/// zero gas; used with gross-sorted candidates to stop simulating once no
/// later candidate can win.
pub fn can_still_win(candidate_gross: U256, incumbent: &(Opportunity, U256)) -> bool {
    candidate_gross > incumbent.0.profit.saturating_sub(incumbent.1)
}

/// The best two-venue round trip of a scan, ignoring `min_profit`. Unlike
/// `Opportunity`, the margin may be negative (a loss) — this is a diagnostic
/// so operators can tell "spread was -0.02 WETH" apart from "spread was -5
/// WETH" when tuning `min_profit`/`loan_amounts`.
pub struct Candidate {
    pub first: usize,
    pub second: usize,
    pub loan_amount: U256,
    pub amount_out: U256,
}

/// Scan every ordered venue pair and loan size and return the outcome with
/// the largest signed margin (`amount_out - loan_amount`), regardless of
/// profitability. Returns None when no pair could be priced end-to-end.
pub fn best_candidate(loan_amounts: &[U256], venues: &[VenueQuotes]) -> Option<Candidate> {
    let mut best: Option<Candidate> = None;
    for first in venues {
        for (i, &loan_amount) in loan_amounts.iter().enumerate() {
            let Some(quote_out) = first.leg1.get(i).copied().and_then(|(q, _)| q) else {
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
                    .and_then(|(_, (out, _))| *out)
                else {
                    continue;
                };
                // Signed-margin comparison without signed ints: a profit
                // (even 0) always beats any loss; among profits the larger
                // wins; among losses the SMALLER magnitude wins.
                let better = match &best {
                    None => true,
                    Some(b) => match (amount_out >= loan_amount, b.amount_out >= b.loan_amount) {
                        (true, false) => true,
                        (false, true) => false,
                        (true, true) => amount_out - loan_amount > b.amount_out - b.loan_amount,
                        (false, false) => loan_amount - amount_out < b.loan_amount - b.amount_out,
                    },
                };
                if better {
                    best = Some(Candidate {
                        first: first.venue,
                        second: second.venue,
                        loan_amount,
                        amount_out,
                    });
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
    // V2/Aero quotes come from constant-product math over reserves fetched
    // fresh on-chain each scan, so there is no persistent local state to
    // validate — provenance is false (not local CL).
    let leg1 = loan_amounts
        .iter()
        .map(|&size| {
            (
                get_amount_out(size, reserves.reserve_in, reserves.reserve_out, fee_bps),
                false,
            )
        })
        .collect();
    let leg2 = leg2_inputs
        .iter()
        .map(|&q| {
            (
                q,
                (
                    get_amount_out(q, reserves.reserve_out, reserves.reserve_in, fee_bps),
                    false,
                ),
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
        let opp = ranked_opportunities(&sizes(&[10_000]), &venues, U256::ZERO)
            .into_iter()
            .next();
        assert!(opp.is_none(), "identical pools must not be profitable");
    }

    #[test]
    fn price_dislocation_yields_profit_in_one_direction() {
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_100_000, 900_000)], &[10_000]);
        let opp = ranked_opportunities(&sizes(&[10_000]), &venues, U256::ZERO)
            .into_iter()
            .next()
            .expect("dislocated pools should yield an opportunity");
        assert_eq!(opp.first, 0);
        assert_eq!(opp.second, 1);
        assert!(opp.profit > U256::ZERO);
        assert_eq!(opp.amount_out, opp.loan_amount + opp.profit);
    }

    #[test]
    fn mirror_direction_is_found_when_pools_are_swapped() {
        let venues = pool_set(&[(1_100_000, 900_000), (1_000_000, 1_000_000)], &[10_000]);
        let opp = ranked_opportunities(&sizes(&[10_000]), &venues, U256::ZERO)
            .into_iter()
            .next()
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
        let opp = ranked_opportunities(&sizes(&[10_000]), &venues, U256::ZERO)
            .into_iter()
            .next()
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
        venues[1].leg1 = vec![(None, false)];
        venues[1].leg2 = venues[1]
            .leg2
            .iter()
            .map(|&(q, _)| (q, (None, false)))
            .collect();
        let opp = ranked_opportunities(&sizes(&[10_000]), &venues, U256::ZERO)
            .into_iter()
            .next()
            .expect("one dead venue must not abort the search");
        assert!(opp.profit > U256::ZERO);
        assert!(opp.first != 1 && opp.second != 1);
    }

    #[test]
    fn min_profit_threshold_filters_small_gains() {
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_001_000, 999_000)], &[10_000]);
        let opp = ranked_opportunities(&sizes(&[10_000]), &venues, U256::from(1_000_000u64))
            .into_iter()
            .next();
        assert!(
            opp.is_none(),
            "tiny dislocation must fail a large min_profit"
        );
    }

    fn opp(first: usize, second: usize, gross: u64) -> Opportunity {
        Opportunity {
            first,
            second,
            loan_amount: U256::from(1_000u64),
            quote_out: U256::ZERO,
            amount_out: U256::from(1_000u64 + gross),
            profit: U256::from(gross),
            leg1_local: false,
            leg2_local: false,
        }
    }

    #[test]
    fn pick_best_net_prefers_lower_gross_lower_gas_when_net_wins() {
        // A: gross 0.020, gas 0.018 → net 0.002. B: gross 0.019, gas 0.002 →
        // net 0.017. Ranking by gross alone picks A (the old behavior); net
        // must pick B.
        let results = vec![
            (opp(0, 1, 20), GasOutcome::Priced(U256::from(18u64))),
            (opp(1, 0, 19), GasOutcome::Priced(U256::from(2u64))),
        ];
        let (chosen, gas) = pick_best_net(results, U256::ZERO).unwrap();
        assert_eq!((chosen.first, chosen.second), (1, 0));
        assert_eq!(gas, U256::from(2u64));
    }

    #[test]
    fn pick_best_net_skips_rejected_and_filters_min_profit() {
        // Top-gross candidate reverts in simulation; the next one must win.
        let results = vec![
            (opp(0, 1, 20), GasOutcome::Rejected),
            (opp(1, 0, 19), GasOutcome::Priced(U256::from(2u64))),
        ];
        let (chosen, _) = pick_best_net(results, U256::ZERO).unwrap();
        assert_eq!((chosen.first, chosen.second), (1, 0));

        // And min_profit filters after gas, not before it.
        let results = vec![(opp(0, 1, 20), GasOutcome::Priced(U256::from(18u64)))];
        assert!(pick_best_net(results, U256::from(3u64)).is_none());
    }

    #[test]
    fn can_still_win_stops_when_gross_cannot_beat_incumbent_net() {
        let incumbent = (opp(0, 1, 20), U256::from(18u64)); // net 2
        assert!(can_still_win(U256::from(3u64), &incumbent));
        assert!(!can_still_win(U256::from(2u64), &incumbent));
        assert!(!can_still_win(U256::from(1u64), &incumbent));
    }

    #[test]
    fn provenance_flows_from_legs_into_opportunity() {
        // Two venues; venue 1's leg2 is local-CL (true), venue 0's leg1 is
        // Quoter/backfill (false). The opportunity must carry leg1_local =
        // false (venue 0 sold the loan) and leg2_local = true (venue 1
        // bought the quote back).
        let sizes_in = [10_000u128];
        let mut venues = pool_set(&[(1_000_000, 1_000_000), (1_100_000, 900_000)], &sizes_in);
        // Overwrite provenance: venue 0 leg1 Quoter-sourced, venue 1 leg2
        // local-CL-sourced.
        venues[0].leg1 = venues[0].leg1.iter().map(|&(q, _)| (q, false)).collect();
        venues[1].leg2 = venues[1]
            .leg2
            .iter()
            .map(|&(q, (o, _))| (q, (o, true)))
            .collect();
        let best = ranked_opportunities(&sizes(&sizes_in), &venues, U256::ZERO)
            .into_iter()
            .next()
            .expect("a profitable route must exist");
        // The profitable direction is first=0 (cheap loan sale) then
        // second=1 (buy back). Confirm provenance attached accordingly.
        assert!(!best.leg1_local, "venue 0 leg1 was Quoter-sourced");
        assert!(best.leg2_local, "venue 1 leg2 was local-CL-sourced");
    }

    #[test]
    fn best_of_multiple_sizes_is_selected() {
        let sizes_in = [1_000u128, 10_000, 100_000];
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_100_000, 900_000)], &sizes_in);
        let best = ranked_opportunities(&sizes(&sizes_in), &venues, U256::ZERO)
            .into_iter()
            .next()
            .expect("some size should be profitable");
        assert!(best.profit > U256::ZERO);
        // The optimum is not necessarily the largest size: price impact
        // grows with size, so the search must consider all of them. Each
        // single-size run needs quotes built for that size alone (leg
        // outputs are aligned with the sizes they were quoted for).
        for &s in &sizes_in {
            let single = pool_set(&[(1_000_000, 1_000_000), (1_100_000, 900_000)], &[s]);
            let only = ranked_opportunities(&sizes(&[s]), &single, U256::ZERO)
                .into_iter()
                .next()
                .unwrap();
            assert!(best.profit >= only.profit);
        }
    }

    #[test]
    fn ranked_opportunities_returns_all_candidates_sorted_by_gross() {
        // Two dislocated pools, two sizes: both directions profitable at
        // some sizes. The list must contain every profitable route, sorted
        // by gross profit descending, so the caller can walk down it after
        // gas instead of being locked into the top-gross candidate.
        let venues = pool_set(
            &[(1_000_000, 1_000_000), (1_100_000, 900_000)],
            &[1_000, 10_000],
        );
        let ranked = ranked_opportunities(&sizes(&[1_000, 10_000]), &venues, U256::ZERO);
        assert!(ranked.len() > 1, "several routes are profitable here");
        assert!(ranked.windows(2).all(|w| w[0].profit >= w[1].profit));
        // min_profit filters the tail, not just the top.
        let threshold = ranked[0].profit;
        let filtered = ranked_opportunities(&sizes(&[1_000, 10_000]), &venues, threshold);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].profit, threshold);
    }

    #[test]
    fn v2_quotes_wire_both_legs() {
        let q = v2_venue(0, 1_000_000, 1_000_000, &[1_000], &[500], 30);
        assert_eq!(q.leg1.len(), 1);
        assert!(q.leg1[0].0.is_some());
        assert_eq!(q.leg2.len(), 1);
        assert_eq!(q.leg2[0].0, U256::from(500u64));
        assert!(q.leg2[0].1 .0.is_some());
    }

    #[test]
    fn best_candidate_picks_max_margin_across_pairs() {
        // Two dislocated pools: route 0→1 and 1→0 have different margins;
        // best_candidate must return the larger one, ignoring min_profit.
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_100_000, 900_000)], &[10_000]);
        let c = best_candidate(&sizes(&[10_000]), &venues).expect("a candidate exists");
        assert!(c.amount_out > c.loan_amount);
        // It must match what ranked_opportunities would pick with no threshold.
        let opp = ranked_opportunities(&sizes(&[10_000]), &venues, U256::ZERO)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!((c.first, c.second), (opp.first, opp.second));
        assert_eq!(c.amount_out, opp.amount_out);
    }

    #[test]
    fn best_candidate_reports_loss_when_no_route_is_profitable() {
        // Identical pools: every round trip loses to the fee. The best
        // candidate is the smallest loss, not None.
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_000_000, 1_000_000)], &[10_000]);
        assert!(ranked_opportunities(&sizes(&[10_000]), &venues, U256::ZERO).is_empty());
        let c = best_candidate(&sizes(&[10_000]), &venues).expect("a losing candidate exists");
        assert!(c.amount_out < c.loan_amount, "fees guarantee a loss");
        // ~2.5% round trip: two 0.3% fees plus price impact of a 1%-of-reserves swap.
        let loss = c.loan_amount - c.amount_out;
        assert!(loss > U256::from(200u64) && loss < U256::from(300u64));
    }

    #[test]
    fn best_candidate_prefers_smallest_loss_across_sizes() {
        // Identical pools, two sizes: the bigger swap has worse price impact,
        // so it loses more. best_candidate must rank the smaller loss above
        // the larger one (regression test for the absolute-magnitude bug).
        let venues = pool_set(
            &[(1_000_000, 1_000_000), (1_000_000, 1_000_000)],
            &[10_000, 100_000],
        );
        let c = best_candidate(&sizes(&[10_000, 100_000]), &venues).expect("a candidate exists");
        assert!(c.amount_out < c.loan_amount);
        assert_eq!(
            c.loan_amount,
            U256::from(10_000u64),
            "the least-negative margin is the smaller size, not the larger loss"
        );
    }

    #[test]
    fn best_candidate_none_when_no_pair_is_pricable() {
        let venues = pool_set(&[(1_000_000, 1_000_000), (1_100_000, 900_000)], &[10_000])
            .into_iter()
            .map(|mut v| {
                v.leg2 = v.leg2.iter().map(|&(q, _)| (q, (None, false))).collect();
                v
            })
            .collect::<Vec<_>>();
        assert!(best_candidate(&sizes(&[10_000]), &venues).is_none());
    }
}
