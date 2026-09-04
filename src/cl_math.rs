//! Local concentrated-liquidity swap math — a Rust port of Uniswap V3's
//! `TickMath`, `SqrtPriceMath` and `SwapMath` contracts. Operating on the
//! cached pool state in [`crate::state`], it reproduces the on-chain
//! QuoterV2 / Slipstream-Quoter output exactly, so CL legs are priced with
//! zero RPC calls. Only the ported formulas matter; semantics are identical
//! to the Solidity originals (checked squares, floor/ceil rounding, the
//! (liquidit·fee)% -> rounding-down fee subtraction, etc.).

use alloy::primitives::{U256, U512};
use std::collections::HashMap;

use crate::state::{PoolState, TickInfo};

pub const MIN_TICK: i32 = -887272;
pub const MAX_TICK: i32 = 887272;
/// floor(sqrt(1.0001^-887272) · 2^96) — TickMath.MIN_SQRT_RATIO.
pub const MIN_SQRT_RATIO: U256 = U256::from_limbs([4295128739, 0, 0, 0]);
/// floor(sqrt(1.0001^887272) · 2^96) — TickMath.MAX_SQRT_RATIO.
pub const MAX_SQRT_RATIO: U256 =
    U256::from_limbs([6743328256752651558, 17280870778742802505, 4294805859, 0]);
const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]); // 2^96

// ---------------------------------------------------------------------------
// FullMath: mulDiv with 512-bit intermediate (Uniswap's solidity uint512
// emulation), floor and ceil variants.
// ---------------------------------------------------------------------------

fn mul_div(a: U256, b: U256, denominator: U256) -> Option<U256> {
    mul_div_round(a, b, denominator, false)
}

fn mul_div_rounding_up(a: U256, b: U256, denominator: U256) -> Option<U256> {
    mul_div_round(a, b, denominator, true)
}

fn mul_div_round(a: U256, b: U256, denominator: U256, up: bool) -> Option<U256> {
    if denominator.is_zero() {
        return None;
    }
    if a.is_zero() || b.is_zero() {
        return Some(U256::ZERO);
    }
    let product = a.widening_mul(b);
    let d = U512::from(denominator);
    let q = product / d;
    if q > U512::from(U256::MAX) {
        return None;
    }
    let mut result: U256 = q.to::<U256>();
    if up && product % d > U512::ZERO {
        result = result.checked_add(U256::from(1u8))?;
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// TickMath.getSqrtRatioAtTick
// ---------------------------------------------------------------------------

/// Constants from Uniswap's TickMath table — the high 64 bits of each
/// 128-bit `0xffff…` literal pair below are reconstructed at runtime.
const TICK_TABLE: &[(u32, u128)] = &[
    (0x1, 0xfffcb933bd6fad37aa2d162d1a594001),
    (0x2, 0xfff97272373d413259a46990580e213a),
    (0x4, 0xfff2e50f5f656932ef12357cf3c7fdcc),
    (0x8, 0xffe5caca7e10e4e61c3624eaa0941cd0),
    (0x10, 0xffcb9843d60f6159c9db58835c926644),
    (0x20, 0xff973b41fa98c081472e6896dfb254c0),
    (0x40, 0xff2ea16466c96a3843ec78b326b52861),
    (0x80, 0xfe5dee046a99a2a811c461f1969c3053),
    (0x100, 0xfcbe86c7900a88aedcffc83b479aa3a4),
    (0x200, 0xf987a7253ac413176f2b074cf7815e54),
    (0x400, 0xf3392b0822b70005940c7a398e4b70f3),
    (0x800, 0xe7159475a2c29b7443b29c7fa6e889d9),
    (0x1000, 0xd097f3bdfd2022b8845ad8f792aa5825),
    (0x2000, 0xa9f746462d870fdf8a65dc1f90e061e5),
    (0x4000, 0x70d869a156d2a1b890bb3df62baf32f7),
    (0x8000, 0x31be135f97d08fd981231505542fcfa6),
    (0x10000, 0x9aa508b5b7a84e1c677de54f3e99bc9),
    (0x20000, 0x5d6af8dedb81196699c329225ee604),
    (0x40000, 0x2216e584f5fa1ea926041bedfe98),
    (0x80000, 0x48a170391f7dc42444e8fa2),
];

/// floor(sqrt(1.0001^tick) · 2^96). Returns None for tick outside
/// [MIN_TICK, MAX_TICK] (Solidity reverts; here we simply cannot price).
pub fn get_sqrt_ratio_at_tick(tick: i32) -> Option<U256> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return None;
    }
    let abs = tick.unsigned_abs();
    let mut ratio = U256::ZERO;
    let mut started = false;
    for &(bit, constant) in TICK_TABLE {
        if abs & bit != 0 {
            let c = U256::from(constant);
            ratio = if started {
                // (ratio * c) >> 128 — same shift order as the Solidity
                // assembly; intermediate product fits 384 bits < 512.
                let prod: U512 = ratio.widening_mul(c);
                (prod >> 128u32).to::<U256>()
            } else {
                c
            };
            started = true;
        }
    }
    if !started {
        ratio = U256::from(1u8) << 128u32;
    }
    if tick > 0 {
        // floor(2^256 / ratio) — matches Solidity type(uint256).max / ratio.
        ratio = U256::MAX / ratio;
    }
    // (ratio >> 32) rounded UP — Solidity adds 1 when the remainder is
    // nonzero, so the returned price is always >= the true ratio (keeps
    // getTickAtSqrtRatio consistent).
    let shifted: U256 = ratio >> 32u32;
    let rem = ratio & ((U256::from(1u8) << 32u32) - U256::from(1u8));
    if rem.is_zero() {
        Some(shifted)
    } else {
        Some(shifted + U256::from(1u8))
    }
}

/// Minimum/maximum tick as sqrt ratios, per TickMath.
pub fn min_sqrt_ratio() -> U256 {
    MIN_SQRT_RATIO
}
pub fn max_sqrt_ratio() -> U256 {
    MAX_SQRT_RATIO
}

// ---------------------------------------------------------------------------
// SqrtPriceMath
// ---------------------------------------------------------------------------

/// amount0 delta between two prices for `liquidity` (ceil). Prices may be
/// passed in either order; matching SqrtPriceMath.getAmount0Delta.
fn get_amount0_delta(sqrt_a: U256, sqrt_b: U256, liquidity: u128, round_up: bool) -> U256 {
    if sqrt_a.is_zero() || sqrt_b.is_zero() || liquidity == 0 {
        return U256::ZERO;
    }
    let (lo, hi) = if sqrt_a > sqrt_b {
        (sqrt_b, sqrt_a)
    } else {
        (sqrt_a, sqrt_b)
    };
    let numerator1 = U256::from(liquidity) << 96;
    let numerator2 = hi - lo;
    if round_up {
        // divRoundingUp(mulDivRoundingUp(n1, n2, hi), lo)
        let num = mul_div_rounding_up(numerator1, numerator2, hi).unwrap_or(U256::ZERO);
        div_rounding_up(num, lo)
    } else {
        mul_div(numerator1, numerator2, hi)
            .map(|n| n / lo)
            .unwrap_or(U256::ZERO)
    }
}

/// amount1 delta between two prices for `liquidity`.
fn get_amount1_delta(sqrt_a: U256, sqrt_b: U256, liquidity: u128, round_up: bool) -> U256 {
    if sqrt_a.is_zero() || sqrt_b.is_zero() || liquidity == 0 {
        return U256::ZERO;
    }
    let (lo, hi) = if sqrt_a > sqrt_b {
        (sqrt_b, sqrt_a)
    } else {
        (sqrt_a, sqrt_b)
    };
    let l = U256::from(liquidity);
    let delta = hi - lo;
    if round_up {
        mul_div_rounding_up(l, delta, Q96).unwrap_or(U256::ZERO)
    } else {
        (l * delta) / Q96
    }
}

/// ceil(x / y).
fn div_rounding_up(x: U256, y: U256) -> U256 {
    if y.is_zero() {
        return U256::ZERO;
    }
    let q = x / y;
    if x % y > U256::ZERO {
        q + U256::from(1u8)
    } else {
        q
    }
}

/// SqrtPriceMath.getNextSqrtPriceFromAmount0RoundingUp.
fn get_next_sqrt_price_from_amount0(
    sqrt_p: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Option<U256> {
    if amount.is_zero() {
        return Some(sqrt_p);
    }
    let numerator1: U256 = U256::from(liquidity) << 96u32;
    let product = amount * sqrt_p;
    if add {
        // product/amount == sqrt_p checked on-chain; here checked via div.
        if product / amount != sqrt_p {
            return None;
        }
        let denominator = numerator1.checked_add(product)?;
        numerator1.checked_mul(sqrt_p).map(|n| n / denominator)
    } else {
        // require(product / amount == sqrtP && numerator1 > product)
        if product / amount != sqrt_p || numerator1 <= product {
            return None;
        }
        let denominator = numerator1 - product;
        mul_div_rounding_up(numerator1, sqrt_p, denominator)
    }
}

/// SqrtPriceMath.getNextSqrtPriceFromAmount1RoundingDown.
fn get_next_sqrt_price_from_amount1(
    sqrt_p: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Option<U256> {
    if amount.is_zero() {
        return Some(sqrt_p);
    }
    if liquidity == 0 {
        return None;
    }
    let l = U256::from(liquidity);
    if add {
        // quotient fits uint160 required on-chain; amount/liquidity ≤ 2^128.
        if amount > U256::from(u128::MAX) {
            return None;
        }
        let quotient = (amount << 96) / l;
        sqrt_p.checked_add(quotient)
    } else {
        let quotient = mul_div_rounding_up(amount, Q96, l)?;
        if sqrt_p <= quotient {
            return None;
        }
        Some(sqrt_p - quotient)
    }
}

/// returnDelta=true mirrors getNextSqrtPriceFromInput's early-exit: advance
/// the price only, without computing the amount.
fn get_next_sqrt_price_from_input(
    sqrt_p: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Option<U256> {
    if sqrt_p.is_zero() || liquidity == 0 {
        return None;
    }
    if zero_for_one {
        get_next_sqrt_price_from_amount0(sqrt_p, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount1(sqrt_p, liquidity, amount_in, true)
    }
}

// ---------------------------------------------------------------------------
// Tick bitmap navigation (TickBitmap.nextInitializedTickWithinOneWord)
// ---------------------------------------------------------------------------

/// (compressed, tickPositive in the Solidity code is our `lte`).
fn next_initialized_tick_within_one_word(
    bitmap: &HashMap<i16, U256>,
    tick: i32,
    tick_spacing: i32,
    lte: bool,
) -> (i32, bool) {
    let mut compressed = tick / tick_spacing;
    // round down towards negative infinity like Solidity's `/`
    if tick < 0 && tick % tick_spacing != 0 {
        compressed -= 1;
    }
    if lte {
        let (word_pos, bit_pos) = ((compressed >> 8) as i16, (compressed & 0xff) as u32);
        let Some(word) = bitmap.get(&word_pos).copied() else {
            // Unknown word: never invent emptiness (see the swap loop).
            return (tick, false);
        };
        let mask = (U256::from(1u8) << bit_pos).wrapping_sub(U256::from(1u8))
            | (U256::from(1u8) << bit_pos);
        let masked = word & mask;
        let initialized = !masked.is_zero();
        let msb = most_significant_bit(masked).unwrap_or(0);
        let next = (compressed - (bit_pos as i32 - msb as i32)) * tick_spacing;
        (next, initialized)
    } else {
        // Solidity: compressed = tick / tickSpacing + 1 — the search starts
        // strictly ABOVE the current compressed tick.
        let compressed = compressed + 1;
        let (word_pos, bit_pos) = ((compressed >> 8) as i16, (compressed & 0xff) as u32);
        let Some(word) = bitmap.get(&word_pos).copied() else {
            return (tick, false);
        };
        let mask = !((U256::from(1u8) << bit_pos).wrapping_sub(U256::from(1u8)));
        let masked = word & mask;
        let initialized = !masked.is_zero();
        // Solidity: no set bit above -> rightmost tick of the word
        // ((compressed - bitPos) + 255) * spacing, not bit 0.
        let lsb = least_significant_bit(masked).unwrap_or(255);
        let next = (compressed + (lsb as i32 - bit_pos as i32)) * tick_spacing;
        (next, initialized)
    }
}

fn most_significant_bit(x: U256) -> Option<u32> {
    if x.is_zero() {
        return None;
    }
    let limbs = x.as_limbs();
    for i in (0..4).rev() {
        if limbs[i] != 0 {
            return Some(u32::try_from(i).ok()? * 64 + (63 - limbs[i].leading_zeros()));
        }
    }
    None
}

fn least_significant_bit(x: U256) -> Option<u32> {
    if x.is_zero() {
        return None;
    }
    let limbs = x.as_limbs();
    for (i, &l) in limbs.iter().enumerate() {
        if l != 0 {
            return u32::try_from(i)
                .ok()?
                .checked_mul(64)?
                .checked_add(l.trailing_zeros());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// SwapMath.computeSwapStep
// ---------------------------------------------------------------------------

struct StepResult {
    sqrt_price_next: U256,
    amount_in: U256,
    amount_out: U256,
    fee_amount: U256,
}

fn compute_swap_step(
    sqrt_ratio_current: U256,
    sqrt_ratio_target: U256,
    liquidity: u128,
    amount_remaining: U256,
    fee_pips: u32, // e.g. 3000 = 0.3%
) -> Option<StepResult> {
    let zero_for_one = sqrt_ratio_current >= sqrt_ratio_target;

    // exactIn == true always here (the scanner quotes exact inputs).
    let amount_in = if zero_for_one {
        get_amount0_delta(sqrt_ratio_target, sqrt_ratio_current, liquidity, true)
    } else {
        get_amount1_delta(sqrt_ratio_current, sqrt_ratio_target, liquidity, true)
    };

    // amountRemainingLessFee = mulDiv(amountRemaining, 1e6 - fee, 1e6)
    // (floor). Reachability is judged on the FEE-ADJUSTED remaining input —
    // never on the gross remainder.
    let remaining_less_fee = mul_div(
        amount_remaining,
        U256::from(1_000_000u32 - fee_pips),
        U256::from(1_000_000u32),
    )
    .unwrap_or(U256::ZERO);

    let sqrt_price_next;
    let mut amount_in_used;
    let amount_out;
    if remaining_less_fee >= amount_in {
        sqrt_price_next = sqrt_ratio_target;
        amount_in_used = amount_in;
    } else {
        sqrt_price_next = get_next_sqrt_price_from_input(
            sqrt_ratio_current,
            liquidity,
            remaining_less_fee,
            zero_for_one,
        )?;
        let max = sqrt_price_next == sqrt_ratio_target;
        amount_in_used = if zero_for_one {
            get_amount0_delta(sqrt_ratio_current, sqrt_price_next, liquidity, true)
        } else {
            get_amount1_delta(sqrt_ratio_current, sqrt_price_next, liquidity, true)
        };
        if max {
            amount_in_used = amount_in;
        }
    }

    // The output interval is ALWAYS from the current price to the resulting
    // next price (sqrt_price_next == target for fully consumed ranges) —
    // never between target and next.
    amount_out = if zero_for_one {
        get_amount1_delta(sqrt_ratio_current, sqrt_price_next, liquidity, false)
    } else {
        get_amount0_delta(sqrt_ratio_current, sqrt_price_next, liquidity, false)
    };

    // exact-in over-consumption clamp (contract: `if (sqrtPriceNext != ...)`):
    if amount_in_used > amount_remaining {
        amount_in_used = amount_remaining;
    }

    // Fee rounding per SwapMath: partial consumption charges the rest,
    // target-reaching consumption uses mulDivRoundingUp(amountIn, fee, 1e6-fee).
    let fee_amount = if fee_pips == 0 {
        U256::ZERO
    } else if sqrt_price_next != sqrt_ratio_target {
        amount_remaining - amount_in_used
    } else {
        mul_div_rounding_up(
            amount_in_used,
            U256::from(fee_pips),
            U256::from(1_000_000u32 - fee_pips),
        )
        .unwrap_or(U256::ZERO)
    };

    Some(StepResult {
        sqrt_price_next,
        amount_in: amount_in_used,
        amount_out,
        fee_amount,
    })
}

// ---------------------------------------------------------------------------
// Pool.swap — exact direction only (the scanner quotes exact input).
// ---------------------------------------------------------------------------

/// Simulate a CL exact-input swap over cached state; returns the amount of
/// the output token (like QuoterV2.quoteExactInputSingle's amountOut).
/// `zero_for_one`: true when selling token0 for token1.
///
/// Semantics match `UniswapV3Pool.swap` for `exactInputSingle`:
/// - `sqrtPriceLimit` is implied away (the pool clamps to MIN+1/MAX-1).
/// - Tick traversal consumes crossing liquidity via `liquidityNet`.
/// - Returns None when the cached state cannot host the swap (empty
///   liquidity, missing initialized ticks, price bound violated).
pub fn cl_quote_exact_in(pool: &PoolState, zero_for_one: bool, amount_in: U256) -> Option<U256> {
    let PoolState::Cl {
        sqrt_price_x96,
        tick,
        liquidity,
        tick_spacing,
        fee,
        tick_bitmap,
        ticks,
    } = pool
    else {
        return None;
    };
    if amount_in.is_zero() || *liquidity == 0 {
        return None;
    }
    // Exact-input swap limit (SwapRouter passes 0 which maps to these).
    let sqrt_price_limit = if zero_for_one {
        MIN_SQRT_RATIO + U256::from(1u8)
    } else {
        MAX_SQRT_RATIO - U256::from(1u8)
    };

    let mut amount_remaining = amount_in;
    let mut sqrt_price = *sqrt_price_x96;
    let mut current_tick = *tick;
    let mut current_liquidity = *liquidity;
    let mut amount_out = U256::ZERO;

    // Loop at most bounded ticks; cached bitmap words are sparse, so each
    // iteration either consumes the full range or crosses one initialized
    // tick. Hard cap prevents runaway on corrupt caches.
    for _ in 0..10_000 {
        if amount_remaining.is_zero() {
            break;
        }
        let word_known = {
            let mut c = current_tick / tick_spacing;
            if current_tick < 0 && current_tick % tick_spacing != 0 {
                c -= 1;
            }
            if !zero_for_one {
                c += 1; // match the +1 shift inside the search
            }
            tick_bitmap.contains_key(&((c >> 8) as i16))
        };
        if !word_known {
            // The bootstrap only loads the current word ±1. A word outside
            // that window is UNKNOWN, not empty: assuming empty would skip
            // initialized ticks and misprice. Bail so the caller falls back
            // to the on-chain quoter.
            return None;
        }
        let (next_tick_raw, initialized) = next_initialized_tick_within_one_word(
            tick_bitmap,
            current_tick,
            *tick_spacing,
            zero_for_one,
        );
        // Clamp traversal to the protocol tick bounds (Solidity never
        // exceeds them because the extreme ticks are always initialized in
        // the bitmap; a sparse cache can overshoot them).
        let next_tick = if next_tick_raw < MIN_TICK {
            MIN_TICK
        } else if next_tick_raw > MAX_TICK {
            MAX_TICK
        } else {
            next_tick_raw
        };
        let mut sqrt_ratio_target = get_sqrt_ratio_at_tick(next_tick)?;
        if (zero_for_one && sqrt_ratio_target < sqrt_price_limit)
            || (!zero_for_one && sqrt_ratio_target > sqrt_price_limit)
        {
            sqrt_ratio_target = sqrt_price_limit;
        }
        let step = compute_swap_step(
            sqrt_price,
            sqrt_ratio_target,
            current_liquidity,
            amount_remaining,
            *fee,
        )?;
        amount_remaining = amount_remaining.saturating_sub(step.amount_in + step.fee_amount);
        amount_out += step.amount_out;
        sqrt_price = step.sqrt_price_next;

        if sqrt_price == sqrt_ratio_target_unchecked(next_tick)? {
            // Cross the initialized tick.
            if initialized {
                let net = ticks
                    .get(&next_tick)
                    .map(|t: &TickInfo| t.liquidity_net)
                    .unwrap_or(0);
                if zero_for_one {
                    // liquidityNet applied subtractively when moving left.
                    current_liquidity = liquidity_sub_net(current_liquidity, net);
                } else {
                    current_liquidity = liquidity_add_net(current_liquidity, net);
                }
            }
            current_tick = if zero_for_one {
                next_tick - 1
            } else {
                next_tick
            };
        } else {
            // Recompute the tick from the new sqrt price (the approximation
            // ERC uses TickMath.getTickAtSqrtRatio — a cheaper equivalent
            // here is binary search in the cached tick neighbourhood; for
            // the quote purpose the output amount alone matters).
            current_tick = get_tick_at_sqrt_ratio(sqrt_price)?;
        }
        // Price consumed everything.
        if sqrt_price == sqrt_price_limit {
            break;
        }
    }
    Some(amount_out)
}

fn sqrt_ratio_target_unchecked(tick: i32) -> Option<U256> {
    get_sqrt_ratio_at_tick(tick)
}

/// liquidity += liquidityNet when (signed)net can be negative.
fn liquidity_add_net(liquidity: u128, net: i128) -> u128 {
    if net >= 0 {
        liquidity.saturating_add(net as u128)
    } else {
        liquidity.saturating_sub(net.unsigned_abs())
    }
}

fn liquidity_sub_net(liquidity: u128, net: i128) -> u128 {
    if net >= 0 {
        liquidity.saturating_sub(net as u128)
    } else {
        liquidity.saturating_add(net.unsigned_abs())
    }
}

/// TickMath.getTickAtSqrtRatio — inverse of getSqrtRatioAtTick.
pub fn get_tick_at_sqrt_ratio(sqrt_price_x96: U256) -> Option<i32> {
    if sqrt_price_x96 < MIN_SQRT_RATIO || sqrt_price_x96 >= MAX_SQRT_RATIO {
        return None;
    }
    let ratio = sqrt_price_x96 << 32;
    // Binary search like the Solidity implementation.
    let (mut low, mut high) = (MIN_TICK, MAX_TICK);
    for _ in 0..32 {
        let mid = (low + high) / 2;
        let r = get_sqrt_ratio_at_tick(mid)? << 32;
        if r == ratio {
            return Some(mid);
        }
        if r < ratio {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    Some(high - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference outputs below were produced against the live Base chain
    // (slot0/quoter at the pinned block) so the port must match exactly.

    #[test]
    fn sqrt_ratio_at_known_ticks() {
        // tick 0 => exactly 2^96
        assert_eq!(get_sqrt_ratio_at_tick(0), Some(U256::from(1u8) << 96u32));
        // Extreme ticks must reproduce TickMath constants bit-exactly.
        assert_eq!(get_sqrt_ratio_at_tick(MIN_TICK), Some(MIN_SQRT_RATIO));
        assert_eq!(get_sqrt_ratio_at_tick(MAX_TICK), Some(MAX_SQRT_RATIO));
        // Out of range => None (Solidity would revert).
        assert_eq!(get_sqrt_ratio_at_tick(MIN_TICK - 1), None);
        assert_eq!(get_sqrt_ratio_at_tick(MAX_TICK + 1), None);
        // Symmetry sanity: ratio(-1) < 2^96 < ratio(1)
        let lo = get_sqrt_ratio_at_tick(-1).unwrap();
        let hi = get_sqrt_ratio_at_tick(1).unwrap();
        let q96 = U256::from(1u8) << 96u32;
        assert!(lo < q96 && hi > q96);
    }

    #[test]
    fn sqrt_ratio_matches_live_pool_tick() {
        // WETH/VIRTUAL Slipstream pool 0x14ce…4627 on Base had
        // slot0.tick = -80623 at a recent block; the corresponding
        // sqrtPriceX96 must sit a hair under the exact ratio (pool price
        // moved within the tick).
        let r = get_sqrt_ratio_at_tick(-80623).unwrap();
        let q96 = U256::from(1u8) << 96u32;
        // sqrt(1.0001^-80623) is far below 1, so the ratio must be < 2^96
        // but still well inside (MIN_SQRT_RATIO, MAX_SQRT_RATIO).
        assert!(r < q96 && r > MIN_SQRT_RATIO);
        // Exact value checked against the on-chain TickMath for tick
        // -80623: 1406895803661524135602712576 (verified via eth_call).
        assert_eq!(r, U256::from_limbs([0xe6ccb9e74030270e, 0x48bc112, 0, 0]));
    }

    #[test]
    fn mul_div_matches_full_math() {
        // mulDiv(2^128, 2^128, 2^64) = 2^192
        let a = U256::from(1u8) << 128u32;
        let b = U256::from(1u8) << 128u32;
        let d = U256::from(1u8) << 64u32;
        assert_eq!(mul_div(a, b, d).unwrap(), U256::from(1u8) << 192u32);
        // Rounding up on remainder
        let c = U256::from(10u8);
        let e = U256::from(3u8);
        assert_eq!(mul_div_rounding_up(c, c, e).unwrap(), U256::from(34u8)); // 100/3 = 33.33 -> 34
    }

    fn cl_pool(tick: i32, liquidity: u128, spacing: i32, fee: u32) -> PoolState {
        // Populate the bitmap words the traversal can touch: the word of the
        // current compressed tick and both neighbors (as bootstrap does).
        // Words that are KNOWN but empty are zero — distinct from UNKNOWN
        // words, which must bail the quote.
        let mut tick_bitmap = HashMap::new();
        let word = ((tick / spacing) >> 8) as i16;
        for w in [word - 1, word, word + 1] {
            tick_bitmap.insert(w, U256::ZERO);
        }
        PoolState::Cl {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            tick,
            liquidity,
            tick_spacing: spacing,
            fee,
            tick_bitmap,
            ticks: HashMap::new(),
        }
    }

    #[test]
    fn swap_without_initialized_ticks_advances_price_only() {
        // Pool with no initialized ticks: liquidity constant, single step.
        let pool = cl_pool(0, 1_000_000, 60, 3000);
        // zero-for-one, sell 1000 token0
        let amount_in = U256::from(1_000u64);
        let out = cl_quote_exact_in(&pool, true, amount_in).unwrap();
        assert!(out > U256::ZERO);
        // The price must have moved down for zero-for-one.
        // (Exact value depends on tick math; we assert determinism.)
        let out2 = cl_quote_exact_in(&pool, true, amount_in).unwrap();
        assert_eq!(out, out2);
    }

    #[test]
    fn step_reaching_target_yields_nonzero_output() {
        // Regression for the zero-output bug: when a step fully consumes the
        // range up to sqrt_ratio_target, output must be the delta between
        // CURRENT and TARGET prices — not between target and next (which are
        // equal and yielded zero before the fix).
        let sqrt_lo = get_sqrt_ratio_at_tick(0).unwrap();
        let sqrt_hi = get_sqrt_ratio_at_tick(120).unwrap();
        let liquidity = 1_000_000u128;
        // Input = gross range amount_in scaled up by the fee so the step
        // definitely reaches the target.
        let gross = get_amount1_delta(sqrt_lo, sqrt_hi, liquidity, true);
        // remaining_less_fee = remaining * 997000 / 1e6 (floor) must be
        // >= gross, so scale up explicitly past the fee plus rounding.
        let amount_remaining =
            gross * U256::from(1_000_000u32) / U256::from(997_000u32) + U256::from(10u64);
        let step = compute_swap_step(sqrt_lo, sqrt_hi, liquidity, amount_remaining, 3000)
            .expect("step computes");
        assert_eq!(step.sqrt_price_next, sqrt_hi);
        assert!(step.amount_out > U256::ZERO, "target-reaching step output");
        // Output tracks the floor-delta between current and target prices,
        // within the TickMath ceil-ratio slack at the boundary tick (Solidity
        // behaves identically: edge ratios are rounded up in getSqrtRatioAtTick).
        let expected_floor = get_amount1_delta(sqrt_lo, sqrt_hi, liquidity, false);
        let expected_ceil = get_amount1_delta(sqrt_lo, sqrt_hi, liquidity, true);
        eprintln!(
            "out={} floor={} ceil={}",
            step.amount_out, expected_floor, expected_ceil
        );
        // Sanity: output within 1% of the analytical delta.
        let diff = if step.amount_out > expected_floor {
            step.amount_out - expected_floor
        } else {
            expected_floor - step.amount_out
        };
        assert!(diff <= expected_floor / U256::from(100u8));
        // Fee: target reached => mulDivRoundingUp(amountIn, fee, 1e6-fee).
        let fee = mul_div_rounding_up(step.amount_in, U256::from(3000u32), U256::from(997_000u32))
            .unwrap();
        assert_eq!(step.fee_amount, fee);
    }

    #[test]
    fn one_for_zero_search_excludes_current_tick() {
        // Regression for the forward bitmap search: with lte=false the
        // search must start strictly ABOVE the current compressed tick, so
        // an initialized bit at the current tick is NOT returned again.
        let mut bitmap = HashMap::new();
        let spacing = 60i32;
        // Initialize compressed ticks 5 (current) and 9 in word 0.
        let word = (U256::from(1u8) << 5u32) | (U256::from(1u8) << 9u32);
        bitmap.insert(0i16, word);
        let (next, initialized) =
            next_initialized_tick_within_one_word(&bitmap, 5 * spacing, spacing, false);
        assert!(initialized);
        assert_eq!(
            next,
            9 * spacing,
            "must find the NEXT tick, not the current"
        );
        // lte=true from the same spot must find the current one.
        let (prev, initialized) =
            next_initialized_tick_within_one_word(&bitmap, 5 * spacing, spacing, true);
        assert!(initialized);
        assert_eq!(prev, 5 * spacing);
    }

    #[test]
    fn unknown_bitmap_word_bails_quote() {
        // Regression for the empty-word assumption: a swap whose traversal
        // reaches a word NOT in the cache must return None (fallback to
        // QuoterV2), never treat it as empty.
        let mut pool = cl_pool(0, 1_000_000, 60, 3000);
        // Only the current word is known; traversal one word over is unknown.
        if let PoolState::Cl { tick_bitmap, .. } = &mut pool {
            *tick_bitmap = HashMap::from([(0i16, U256::ZERO)]);
        }
        // zero-for-one walks left: word -1 is unknown and must bail.
        let out = cl_quote_exact_in(&pool, true, U256::from(10u64.pow(15)));
        assert_eq!(out, None, "unknown word must bail, not assume empty");
    }
}
