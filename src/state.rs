//! Local, event-driven pool state. Instead of re-reading reserves and
//! quoting through on-chain Quoter contracts on every scan (dozens of
//! `eth_call`s per iteration), the bot keeps an in-memory copy of every
//! pool's state and updates it from the `Sync`/`Swap`/`Mint`/`Burn` logs it
//! already subscribes to — including Flashblock `pendingLogs`, which makes
//! the cache fresh every ~200ms at zero RPC cost. V2 swaps are priced with
//! constant-product math (see [`crate::dex::get_amount_out`]); CL pools
//! (Uniswap V3 / Aerodrome Slipstream) are priced with local tick-math (see
//! [`crate::cl_math`]). RPC reads remain only for bootstrap and periodic
//! re-snapshots.

use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol_types::SolCall;
use eyre::Result;
use std::collections::HashMap;

use crate::dex::IClPoolState;

/// One initialized tick of a CL pool, as read from `ticks(int24)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TickInfo {
    /// Total liquidity referencing this tick (`liquidityGross`).
    pub liquidity_gross: u128,
    /// Liquidity added/removed when the price crosses the tick
    /// left-to-right (`liquidityNet`, signed).
    pub liquidity_net: i128,
    /// Whether this tick flips a bit in `tick_bitmap` — bounds at least
    /// one open position.
    pub initialized: bool,
}

/// Cached state of one pool. `Cl` covers Uniswap V3 and Aerodrome
/// Slipstream — identical swap math, different deployed contracts.
#[derive(Debug, Clone)]
pub enum PoolState {
    /// Constant-product pool (Uniswap V2, Aerodrome volatile/stable).
    /// Raw token0/token1 reserves; orientation applied by the caller.
    V2 { reserve0: U256, reserve1: U256 },
    /// Concentrated-liquidity pool (Uniswap V3, Aerodrome Slipstream).
    Cl {
        /// `slot0.sqrtPriceX96`.
        sqrt_price_x96: U256,
        /// `slot0.tick`.
        tick: i32,
        /// In-range liquidity (`liquidity()`).
        liquidity: u128,
        /// Immutable pool tick spacing.
        tick_spacing: i32,
        /// Pool fee in hundredths of a bip (3000 = 0.3%).
        fee: u32,
        /// `tickBitmap(int16)` words, keyed by word position.
        tick_bitmap: HashMap<i16, U256>,
        /// Initialized ticks (bootstrap + Mint/Burn).
        ticks: HashMap<i32, TickInfo>,
    },
}

impl PoolState {
    /// Fold an overlay PoolState (the post-state after preconfirmed events)
    /// into `self`: CL inventory fields are absolute post-values, and tick /
    /// bitmap deltas were already applied to the overlay's own maps.
    pub fn merge(&mut self, ov: &PoolState) {
        if let (
            PoolState::Cl {
                sqrt_price_x96,
                tick,
                liquidity,
                tick_bitmap,
                ticks,
                ..
            },
            PoolState::Cl {
                sqrt_price_x96: o_sp,
                tick: o_t,
                liquidity: o_l,
                tick_bitmap: o_bm,
                ticks: o_tk,
                ..
            },
        ) = (self, ov)
        {
            *sqrt_price_x96 = *o_sp;
            *tick = *o_t;
            *liquidity = *o_l;
            for (w, v) in o_bm {
                tick_bitmap.insert(*w, *v);
            }
            for (t, info) in o_tk {
                ticks.insert(*t, info.clone());
            }
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self {
            PoolState::V2 { .. } => "v2",
            PoolState::Cl { .. } => "cl",
        }
    }
}

/// Decoded V2 `Sync` event.
#[derive(Debug, Clone, Copy)]
pub struct SyncEvent {
    pub reserve0: U256,
    pub reserve1: U256,
}

/// Decoded CL `Swap` event: absolute post-swap inventory.
#[derive(Debug, Clone, Copy)]
pub struct ClSwapEvent {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
}

/// Decoded CL `Mint`/`Burn` event: liquidity delta over a range.
#[derive(Debug, Clone, Copy)]
pub struct ClLiquidityEvent {
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: u128,
}

/// In-memory store of all tracked pool states.
#[derive(Debug)]
pub struct StateStore {
    pools: HashMap<Address, PoolState>,
    /// Wall-clock of the last applied event, for staleness diagnostics.
    pub last_event_at: Option<std::time::Instant>,
    /// Wall-clock of the last full re-bootstrap/refresh of this store.
    pub last_refresh_at: std::time::Instant,
    /// Chain block this snapshot reflects: set by bootstrap, advanced by
    /// every applied event, cleared on reorg-drop. Scans refuse to price
    /// from state pinned to a different block than the RPC legs.
    pub block: Option<u64>,
}

impl StateStore {
    pub fn new() -> Self {
        Self {
            pools: HashMap::new(),
            last_event_at: None,
            last_refresh_at: std::time::Instant::now(),
            block: None,
        }
    }

    pub fn insert(&mut self, pool: Address, state: PoolState) {
        self.pools.insert(pool, state);
    }

    pub fn insert_at(&mut self, pool: Address, state: PoolState, block: u64) {
        self.pools.insert(pool, state);
        self.block = Some(block);
        self.last_refresh_at = std::time::Instant::now();
    }

    pub fn get(&self, pool: &Address) -> Option<&PoolState> {
        self.pools.get(pool)
    }

    pub fn remove(&mut self, pool: &Address) {
        self.pools.remove(pool);
        self.block = None;
    }

    fn touch(&mut self) {
        self.last_event_at = Some(std::time::Instant::now());
    }

    /// Record the actual block of an applied event. Several logs in one
    /// block keep the same pin; a later block raises it — never fabricated.
    pub fn advance_to(&mut self, block: u64) {
        self.last_event_at = Some(std::time::Instant::now());
        self.block = Some(self.block.map_or(block, |b| b.max(block)));
    }

    /// Borrowed state when unmodified by `pending`, else the sealed state
    /// with every pending overlay event folded in (cloned). Keeps the
    /// pending overlay as deltas only — no full snapshot duplication.
    pub fn resolved(
        &self,
        pool: &Address,
        pending: &StateStore,
    ) -> Option<std::borrow::Cow<'_, PoolState>> {
        let base = self.pools.get(pool)?;
        match pending.pools.get(pool) {
            None => Some(std::borrow::Cow::Borrowed(base)),
            Some(ov) => {
                let mut st = base.clone();
                st.merge(ov);
                Some(std::borrow::Cow::Owned(st))
            }
        }
    }

    pub fn len(&self) -> usize {
        self.pools.len()
    }

    /// V2 `Sync` applier — reserves carried by the event are absolute.
    pub fn apply_v2_sync(&mut self, pool: Address, ev: SyncEvent) {
        if let Some(PoolState::V2 { reserve0, reserve1 }) = self.pools.get_mut(&pool) {
            *reserve0 = ev.reserve0;
            *reserve1 = ev.reserve1;
            self.touch();
        }
    }

    /// CL `Swap` applier — event carries absolute inventory.
    pub fn apply_cl_swap(&mut self, pool: Address, ev: ClSwapEvent) {
        if let Some(PoolState::Cl {
            sqrt_price_x96,
            tick,
            liquidity,
            ..
        }) = self.pools.get_mut(&pool)
        {
            *sqrt_price_x96 = ev.sqrt_price_x96;
            *tick = ev.tick;
            *liquidity = ev.liquidity;
            self.touch();
        }
    }

    /// CL Mint (positive delta) / Burn (negative delta) applier: adjusts
    /// `liquidityNet`/`liquidityGross` on both bounds, flips bitmap bits
    /// on (de)initialization, and adjusts in-range liquidity when the
    /// current tick sits inside the modified range.
    pub fn apply_cl_liquidity(&mut self, pool: Address, ev: ClLiquidityEvent, is_burn: bool) {
        let delta: i128 = if is_burn {
            -(ev.liquidity as i128)
        } else {
            ev.liquidity as i128
        };
        let Some(PoolState::Cl {
            tick,
            liquidity,
            tick_spacing,
            tick_bitmap,
            ticks,
            ..
        }) = self.pools.get_mut(&pool)
        else {
            return;
        };

        for (boundary, sign) in [(ev.tick_lower, 1i128), (ev.tick_upper, -1i128)] {
            let info = ticks.entry(boundary).or_default();
            info.liquidity_net += sign * delta;
            info.liquidity_gross = if is_burn {
                info.liquidity_gross.saturating_sub(ev.liquidity)
            } else {
                info.liquidity_gross.saturating_add(ev.liquidity)
            };
            let was = info.initialized;
            let now = info.liquidity_gross > 0;
            info.initialized = now;
            if was != now {
                flip_bitmap_bit(tick_bitmap, boundary, *tick_spacing);
            }
        }

        // In-range liquidity changes only when the modified range spans
        // the current tick.
        if ev.tick_lower <= *tick && *tick < ev.tick_upper {
            *liquidity = if is_burn {
                liquidity.saturating_sub(ev.liquidity)
            } else {
                liquidity.saturating_add(ev.liquidity)
            };
        }
        self.touch();
    }
}

/// Flip the bit for `tick` in its `tickBitmap` word (floor division for
/// negative ticks, per Uniswap V3's bitmap convention).
pub fn flip_bitmap_bit(bitmap: &mut HashMap<i16, U256>, tick: i32, spacing: i32) {
    let compressed = tick.div_euclid(spacing);
    let word = compressed.div_euclid(256) as i16;
    let bit = compressed.rem_euclid(256) as u32;
    let entry = bitmap.entry(word).or_insert(U256::ZERO);
    *entry ^= U256::from(1u8) << bit;
}

/// Decode a 32-byte big-endian word holding a sign-extended int24 (the
/// Uniswap V3 tick encoding): the value lives in the low 3 bytes; the high
/// byte of those three is the sign bit.
pub fn decode_i24(word: &[u8]) -> i32 {
    let Some(low) = word.get(word.len().saturating_sub(3)..) else {
        return 0;
    };
    let v = i32::from_be_bytes([0, low[0], low[1], low[2]]);
    if v & 0x80_0000 != 0 {
        v - 0x100_0000
    } else {
        v
    }
}

/// V2 `Sync` data payload: two uint112 in 32-byte words (Aerodrome's
/// uint256 variant decodes identically).
pub fn decode_v2_sync(data: &[u8]) -> Option<SyncEvent> {
    if data.len() < 64 {
        return None;
    }
    Some(SyncEvent {
        reserve0: U256::from_be_slice(&data[..32]),
        reserve1: U256::from_be_slice(&data[32..64]),
    })
}

/// CL `Swap` data payload: (amount0, amount1, sqrtPriceX96, liquidity,
/// tick) — absolute inventory in the last three words.
pub fn decode_cl_swap(data: &[u8]) -> Option<ClSwapEvent> {
    if data.len() < 160 {
        return None;
    }
    let sqrt_price_x96 = U256::from_be_slice(&data[64..96]);
    let liquidity = U256::from_be_slice(&data[96..128]).to::<u128>();
    let tick = decode_i24(&data[128..160]);
    Some(ClSwapEvent {
        sqrt_price_x96,
        liquidity,
        tick,
    })
}

/// CL Mint/Burn canonical layout:
///   Mint(address,address,int24,int24,uint128,uint256,uint256)
///   Burn(address,int24,int24,uint128,uint256,uint256)
/// topics: [sig, owner, tickLower, tickUpper]; data words: Mint carries
/// `(amount0, amount1, amount)` so liquidity is word 2, Burn carries
/// `(amount, amount0, amount1)` so liquidity is word 0.
pub fn decode_cl_liquidity(
    data: &[u8],
    topics: &[B256],
    is_burn: bool,
) -> Option<ClLiquidityEvent> {
    if data.len() < 96 || topics.len() < 4 {
        return None;
    }
    let idx = if is_burn { 0 } else { 2 };
    let liquidity = U256::from_be_slice(&data[idx * 32..idx * 32 + 32]).to::<u128>();
    Some(ClLiquidityEvent {
        tick_lower: decode_i24(topics[2].as_slice()),
        tick_upper: decode_i24(topics[3].as_slice()),
        liquidity,
    })
}

// ---------------------------------------------------------------------------
// Startup bootstrap of CL state via batched eth_calls: (1) slot0 +
// liquidity + fee + tickSpacing, (2) tickBitmap words around the current
// tick, (3) ticks() for every set bit in those words. Free function so
// tests need no provider.
// ---------------------------------------------------------------------------
pub async fn bootstrap_cl<P: Provider>(provider: &P, pool: Address) -> Result<PoolState> {
    // Pin every read to ONE numbered block: successive rounds of eth_call
    // would otherwise observe different blocks when the chain advances
    // mid-bootstrap, mixing slot0/liquidity/bitmap/tick snapshots.
    let block = provider.get_block_number().await?;
    bootstrap_cl_at(provider, pool, block).await
}

/// Bootstrap against an explicit pinned block — used by the scan's
/// periodic state refresh so local CL state matches the exact block the
/// RPC legs of the same scan were priced at.
pub async fn bootstrap_cl_at<P: Provider>(
    provider: &P,
    pool: Address,
    block: u64,
) -> Result<PoolState> {
    let block_id = alloy::eips::BlockId::number(block);
    let mk = |data: alloy::primitives::Bytes| {
        (
            TransactionRequest::default().to(pool).input(data.into()),
            block_id,
        )
    };

    // ---- round 1 ----
    let mut batch = alloy::rpc::client::BatchRequest::new(provider.client());
    let w1 = batch
        .add_call::<_, alloy::primitives::Bytes>(
            "eth_call",
            &mk(IClPoolState::slot0Call {}.abi_encode().into()),
        )
        .map_err(eyre::Error::from)?;
    let w2 = batch
        .add_call::<_, alloy::primitives::Bytes>(
            "eth_call",
            &mk(IClPoolState::liquidityCall {}.abi_encode().into()),
        )
        .map_err(eyre::Error::from)?;
    let w3 = batch
        .add_call::<_, alloy::primitives::Bytes>(
            "eth_call",
            &mk(IClPoolState::feeCall {}.abi_encode().into()),
        )
        .map_err(eyre::Error::from)?;
    let w4 = batch
        .add_call::<_, alloy::primitives::Bytes>(
            "eth_call",
            &mk(IClPoolState::tickSpacingCall {}.abi_encode().into()),
        )
        .map_err(eyre::Error::from)?;
    batch.send().await.map_err(eyre::Error::from)?;
    let slot0_raw = w1.await.map_err(eyre::Error::from)?;
    let liq_raw = w2.await.map_err(eyre::Error::from)?;
    let fee_raw = w3.await.map_err(eyre::Error::from)?;
    let spacing_raw = w4.await.map_err(eyre::Error::from)?;

    // Decode ABI-agnostically: Uniswap V3 and Slipstream slot0/ticks return
    // different arities, but the fields this module needs are always at the
    // same leading positions. slot0 word 0 = sqrtPriceX96, word 1 = tick.
    if slot0_raw.len() < 64 || liq_raw.len() < 32 || fee_raw.len() < 32 || spacing_raw.len() < 32 {
        eyre::bail!("short slot0/liquidity/fee/spacing payload");
    }
    let sqrt_price_x96 = U256::from_be_slice(&slot0_raw[..32]);
    let current_tick = decode_i24(&slot0_raw[32..64]);
    let liquidity_u128 = U256::from_be_slice(&liq_raw[..32]).to::<u128>();
    let fee_u32 = U256::from_be_slice(&fee_raw[..32]).to::<u32>();
    let tick_spacing = decode_i24(&spacing_raw[..32]);
    if tick_spacing <= 0 {
        eyre::bail!("non-positive tickSpacing");
    }

    // ---- round 2: bitmap words [w0-1, w0, w0+1] ----
    let compressed = current_tick.div_euclid(tick_spacing);
    let w0 = compressed.div_euclid(256) as i16;
    let mut batch2 = alloy::rpc::client::BatchRequest::new(provider.client());
    let mut waiters2 = Vec::new();
    for w in [w0 - 1, w0, w0 + 1] {
        waiters2.push(
            batch2
                .add_call::<_, alloy::primitives::Bytes>(
                    "eth_call",
                    &mk(IClPoolState::tickBitmapCall { wordPosition: w }
                        .abi_encode()
                        .into()),
                )
                .map_err(eyre::Error::from)?,
        );
    }
    batch2.send().await.map_err(eyre::Error::from)?;

    let mut tick_bitmap: HashMap<i16, U256> = HashMap::new();
    let mut set_bits: Vec<(i16, u32)> = Vec::new();
    for (w, waiter) in [w0 - 1, w0, w0 + 1].into_iter().zip(waiters2.into_iter()) {
        let raw = waiter.await.map_err(eyre::Error::from)?;
        if raw.len() < 32 {
            eyre::bail!("short tickBitmap payload");
        }
        let val = U256::from_be_slice(&raw[..32]);
        tick_bitmap.insert(w, val);
        for bit in 0..256u32 {
            if (val >> bit) & U256::from(1u8) == U256::from(1u8) {
                set_bits.push((w, bit));
            }
        }
    }

    if set_bits.is_empty() {
        // No initialized ticks in the neighborhood (e.g. Slipstream pairs
        // with zero out-range liquidity) — cache the empty inventory and
        // let events initialize it.
        return Ok(PoolState::Cl {
            sqrt_price_x96,
            tick: current_tick,
            liquidity: liquidity_u128,
            tick_spacing,
            fee: fee_u32,
            tick_bitmap,
            ticks: HashMap::new(),
        });
    }

    // ---- round 3: ticks() per initialized bit ----
    let mut batch3 = alloy::rpc::client::BatchRequest::new(provider.client());
    let mut waiters3 = Vec::new();
    for &(word, bit) in &set_bits {
        let tick = ((word as i32) * 256 + bit as i32) * tick_spacing;
        waiters3.push(
            batch3
                .add_call::<_, alloy::primitives::Bytes>(
                    "eth_call",
                    &mk(IClPoolState::ticksCall {
                        tick: alloy::primitives::aliases::I24::try_from(tick)
                            .map_err(|e| eyre::eyre!("{e}"))?,
                    }
                    .abi_encode()
                    .into()),
                )
                .map_err(eyre::Error::from)?,
        );
    }
    batch3.send().await.map_err(eyre::Error::from)?;

    let mut ticks: HashMap<i32, TickInfo> = HashMap::new();
    for (&(word, bit), waiter) in set_bits.iter().zip(waiters3.into_iter()) {
        let tick = ((word as i32) * 256 + bit as i32) * tick_spacing;
        let raw = waiter.await.map_err(eyre::Error::from)?;
        if raw.len() < 96 {
            eyre::bail!("short ticks() payload");
        }
        ticks.insert(
            tick,
            TickInfo {
                liquidity_gross: U256::from_be_slice(&raw[..32]).to::<u128>(),
                liquidity_net: {
                    let low = raw.get(48..64).unwrap_or(&[]);
                    let mut b = [0u8; 16];
                    let n = low.len().min(16);
                    b[16 - n..].copy_from_slice(&low[low.len() - n..]);
                    i128::from_be_bytes(b)
                },
                initialized: U256::from_be_slice(&raw[raw.len() - 32..]) == U256::from(1u8),
            },
        );
    }

    Ok(PoolState::Cl {
        sqrt_price_x96,
        tick: current_tick,
        liquidity: liquidity_u128,
        tick_spacing,
        fee: fee_u32,
        tick_bitmap,
        ticks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(n: u64) -> Vec<u8> {
        U256::from(n).to_be_bytes::<32>().to_vec()
    }

    fn signed_word(v: i32) -> Vec<u8> {
        // Sign-extend an int24 value into a 32-byte big-endian word.
        let as_i256 = alloy::primitives::aliases::I256::try_from(i64::from(v))
            .unwrap()
            .to_be_bytes::<32>();
        as_i256.to_vec()
    }

    #[test]
    fn decode_v2_sync_extracts_reserves() {
        let mut data = word(1000);
        data.extend(word(2000));
        let ev = decode_v2_sync(&data).unwrap();
        assert_eq!(ev.reserve0, U256::from(1000u64));
        assert_eq!(ev.reserve1, U256::from(2000u64));
    }

    #[test]
    fn decode_cl_swap_uses_last_three_words() {
        let mut data = word(0);
        data.extend(word(0));
        data.extend((U256::from(1u8) << 96u32).to_be_bytes::<32>().to_vec());
        data.extend(word(42));
        data.extend(signed_word(-80623));
        let ev = decode_cl_swap(&data).unwrap();
        assert_eq!(ev.sqrt_price_x96, U256::from(1u8) << 96u32);
        assert_eq!(ev.liquidity, 42);
        assert_eq!(ev.tick, -80623);
    }

    #[test]
    fn decode_cl_liquidity_mint_and_burn_words() {
        let topics = vec![
            B256::ZERO,              // sig
            B256::repeat_byte(0xab), // owner (indexed) — must be ignored
            B256::from_slice(&signed_word(-100)),
            B256::from_slice(&signed_word(200)),
        ];
        let mut mint = word(1);
        mint.extend(word(2));
        mint.extend(word(777));
        let ev = decode_cl_liquidity(&mint, &topics, false).unwrap();
        assert_eq!(
            (ev.tick_lower, ev.tick_upper, ev.liquidity),
            (-100, 200, 777)
        );

        let mut burn = word(777);
        burn.extend(word(1));
        burn.extend(word(2));
        let ev = decode_cl_liquidity(&burn, &topics, true).unwrap();
        assert_eq!(ev.liquidity, 777);
    }

    #[test]
    fn flip_bitmap_bit_targets_correct_word_and_bit() {
        let mut bitmap: HashMap<i16, U256> = HashMap::new();
        flip_bitmap_bit(&mut bitmap, 300, 60);
        assert_eq!(bitmap.get(&0), Some(&(U256::from(1u8) << 5)));
        flip_bitmap_bit(&mut bitmap, -60, 60);
        let word = bitmap.get(&-1).unwrap();
        assert_eq!((*word) >> 255u32, U256::from(1u8));
    }

    #[test]
    fn apply_cl_liquidity_updates_bounds_and_in_range() {
        let pool = Address::ZERO;
        let mut store = StateStore::new();
        store.insert(
            pool,
            PoolState::Cl {
                sqrt_price_x96: U256::ZERO,
                tick: 0,
                liquidity: 100,
                tick_spacing: 60,
                fee: 3000,
                tick_bitmap: HashMap::new(),
                ticks: HashMap::new(),
            },
        );
        let ev = ClLiquidityEvent {
            tick_lower: -60,
            tick_upper: 120,
            liquidity: 50,
        };
        store.apply_cl_liquidity(pool, ev, false);
        let PoolState::Cl {
            liquidity, ticks, ..
        } = store.get(&pool).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(*liquidity, 150);
        assert_eq!(ticks[&-60].liquidity_net, 50);
        assert!(ticks[&-60].initialized);

        store.apply_cl_liquidity(
            pool,
            ClLiquidityEvent {
                tick_lower: -60,
                tick_upper: 120,
                liquidity: 10,
            },
            true,
        );
        let PoolState::Cl {
            liquidity, ticks, ..
        } = store.get(&pool).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(*liquidity, 140);
        assert_eq!(ticks[&-60].liquidity_net, 40);
    }

    #[test]
    fn resolved_merges_pending_overlay_without_touching_base() {
        let pool = Address::ZERO;
        let base_cl = PoolState::Cl {
            sqrt_price_x96: U256::from(100u64),
            tick: 1,
            liquidity: 500,
            tick_spacing: 60,
            fee: 3000,
            tick_bitmap: HashMap::new(),
            ticks: HashMap::new(),
        };
        let mut store = StateStore::new();
        store.insert(pool, base_cl.clone());

        // No overlay: borrowed, identical to sealed state.
        let pending = StateStore::new();
        let r = store.resolved(&pool, &pending).unwrap();
        assert!(matches!(r, std::borrow::Cow::Borrowed(_)));

        // With overlay: merged clone; sealed state untouched.
        let mut ov = base_cl.clone();
        if let PoolState::Cl {
            sqrt_price_x96,
            liquidity,
            ticks,
            ..
        } = &mut ov
        {
            *sqrt_price_x96 = U256::from(999u64);
            *liquidity = 777;
            ticks.insert(
                -60,
                TickInfo {
                    liquidity_gross: 10,
                    liquidity_net: -10,
                    initialized: true,
                },
            );
        }
        let mut pending = StateStore::new();
        pending.insert(pool, ov);
        let r = store.resolved(&pool, &pending).unwrap();
        let PoolState::Cl {
            sqrt_price_x96,
            liquidity,
            ticks,
            ..
        } = r.as_ref()
        else {
            panic!("wrong variant");
        };
        assert_eq!(*sqrt_price_x96, U256::from(999u64));
        assert_eq!(*liquidity, 777);
        assert_eq!(ticks[&-60].liquidity_net, -10);
        // Base remains the sealed snapshot.
        let PoolState::Cl {
            sqrt_price_x96: bsp,
            liquidity: bl,
            ..
        } = store.get(&pool).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(*bsp, U256::from(100u64));
        assert_eq!(*bl, 500);
    }

    #[test]
    fn apply_cl_swap_overwrites_inventory() {
        let pool = Address::ZERO;
        let mut store = StateStore::new();
        store.insert(
            pool,
            PoolState::Cl {
                sqrt_price_x96: U256::from(5u64),
                tick: 1,
                liquidity: 100,
                tick_spacing: 60,
                fee: 3000,
                tick_bitmap: HashMap::new(),
                ticks: HashMap::new(),
            },
        );
        store.apply_cl_swap(
            pool,
            ClSwapEvent {
                sqrt_price_x96: U256::from(7u64),
                liquidity: 250,
                tick: -3,
            },
        );
        let PoolState::Cl {
            sqrt_price_x96,
            tick,
            liquidity,
            ..
        } = store.get(&pool).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(*sqrt_price_x96, U256::from(7u64));
        assert_eq!(*tick, -3);
        assert_eq!(*liquidity, 250);
    }
}
