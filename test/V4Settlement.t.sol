// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {FlashArbitrage} from "../contracts/FlashArbitrage.sol";

/// Minimal cheatcode interface (no forge-std dependency).
interface Vm {
    function expectRevert(bytes calldata) external;
    function deal(address, uint256) external;
}

/// File-scope mirror of the PoolManager interface in FlashArbitrage.sol so
/// the test can name the PoolKey/SwapParams types without importing the
/// production file's interfaces (they are private to that file).
interface IPoolManagerLike {
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    struct SwapParams {
        bool zeroForOne;
        int256 amountSpecified;
        uint160 sqrtPriceLimitX96;
    }

    function unlock(bytes calldata data) external returns (bytes memory);
    function swap(PoolKey memory key, SwapParams memory params, bytes calldata hookData)
        external
        returns (int256 swapDelta);
    function take(address currency, address to, uint256 amount) external;
    function settle() external payable returns (uint256 paid);
    function sync(address currency) external;
}

library PoolKeyChecksum {
    /// Matches the production contract's PoolIdLibrary: PoolKey fields in
    /// the manager's declared order.
    function checksum(address c0, address c1, uint24 fee, int24 tickSpacing, address hooks)
        internal
        pure
        returns (bytes32)
    {
        return keccak256(abi.encode(c0, c1, fee, tickSpacing, hooks));
    }
}

/// Minimal ERC20 so the V4 settle path can move real balances
/// (sync/transfer/settle must touch actual token state).
contract MintableToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

/// Mock PoolManager that returns a caller-set BalanceDelta from `swap`,
/// records every `take`, and routes `unlock` back into the arbitrage
/// contract's `unlockCallback` (exactly how the real manager behaves).
contract MockPoolManager {
    address payable public arbitrage;

    int256 public swapDelta;
    bool public lastZeroForOne;
    int256 public lastAmountSpecified;

    address public lastTakeCurrency;
    address public lastTakeTo;
    uint256 public lastTakeAmount;
    /// ETH the manager paid out for a native-currency `take`.
    uint256 public nativeTaken;
    /// `msg.value` carried by the last `settle`: native debt is settled with
    /// call value, so this is how the manager observes a native input.
    uint256 public lastSettleValue;
    /// `true` when the last `settle` settled an ERC20 (sync-then-transfer)
    /// rather than native currency.
    bool public lastSettleWasErc20;

    address public lastSyncCurrency;

    function setArbitrage(address arb) external {
        arbitrage = payable(arb);
    }

    function setSwapDelta(int256 d) external {
        swapDelta = d;
    }

    receive() external payable {}

    function unlock(bytes calldata data) external returns (bytes memory) {
        // Delegates back into the arbitrage contract; `msg.sender` becomes
        // this manager, matching `_v4Unlocker` set by `_swapV4`.
        FlashArbitrage(arbitrage).unlockCallback(data);
        return "";
    }

    function swap(
        IPoolManagerLike.PoolKey calldata,
        IPoolManagerLike.SwapParams calldata params,
        bytes calldata
    ) external returns (int256) {
        lastZeroForOne = params.zeroForOne;
        lastAmountSpecified = params.amountSpecified;
        return swapDelta;
    }

    function take(address currency, address to, uint256 amount) external {
        lastTakeCurrency = currency;
        lastTakeTo = to;
        lastTakeAmount = amount;
        // PoolManager.transfer for the native currency sends ETH via `call`,
        // which is exactly the path that needs a payable receiver on the
        // arbitrage contract.
        if (currency == address(0)) {
            nativeTaken += amount;
            (bool ok,) = to.call{value: amount}("");
            require(ok, "native take failed");
        }
    }

    function settle() external payable returns (uint256) {
        lastSettleValue = msg.value;
        // Mirrors PoolManager._settle: the synced currency is address(0) for
        // the native path (sync resets it) and is paid with msg.value.
        if (lastSyncCurrency == address(0)) {
            lastSettleWasErc20 = false;
            return msg.value;
        }
        lastSettleWasErc20 = true;
        return 0;
    }

    function sync(address currency) external {
        lastSyncCurrency = currency;
    }
}

/// Exposes the internal `_swapV4` path so the test can drive a single leg
/// without the full Morpho flash-loan flow.
contract FlashArbitrageHarness is FlashArbitrage {
    constructor(address _morpho) FlashArbitrage(_morpho) {}

    function harnessSwap(
        FlashArbitrage.SwapLeg calldata leg,
        address from,
        address to,
        uint256 amountIn
    ) external returns (uint256) {
        return _swapV4(leg, from, to, amountIn);
    }
}

contract V4SettlementTest {
    Vm constant vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);

    address constant MOCK_MORPHO = address(0x0000000000000000000000000000000000000a11);

    function buildLeg(address manager, bool zeroForOne)
        internal
        pure
        returns (FlashArbitrage.SwapLeg memory leg)
    {
        leg = FlashArbitrage.SwapLeg({
            router: manager,
            kind: 3, // KIND_UNISWAP_V4
            factory: address(0),
            stable: false,
            feeTier: 3000,
            poolId: bytes32(0),
            minOut: 0,
            tickSpacing: 60,
            hooks: address(0),
            zeroForOne: zeroForOne
        });
    }

    /// Correct PoolKey construction matches the contract's reconstruction:
    /// currency0/currency1 sorted by address, fee/tickSpacing/hooks from leg.
    function poolKey(address c0, address c1)
        internal
        pure
        returns (IPoolManagerLike.PoolKey memory key)
    {
        (address lo, address hi) = c0 < c1 ? (c0, c1) : (c1, c0);
        key = IPoolManagerLike.PoolKey({
            currency0: lo,
            currency1: hi,
            fee: 3000,
            tickSpacing: 60,
            hooks: address(0)
        });
    }

    /// Pack a BalanceDelta for a zeroForOne swap:
    /// amount0 (negative input) in the upper 128 bits, amount1 (positive
    /// output) in the lower 128 bits.
    function packZ1(int256 amount0, int256 amount1) internal pure returns (int256) {
        // amount0 is the negative input: its 128-bit two's complement is
        // 2^128 - |amount0|. amount1 is the positive output.
        uint256 hi = (uint256(1) << 128) - uint256(-amount0);
        uint256 lo = uint256(amount1);
        return int256((hi << 128) | lo);
    }

    /// Pack a BalanceDelta for a zeroForOne=false swap:
    /// amount1 (negative input) in the lower 128 bits, amount0 (positive
    /// output) in the upper 128 bits.
    function packZ0(int256 amount0, int256 amount1) internal pure returns (int256) {
        uint256 hi = uint256(amount0);
        uint256 lo = (uint256(1) << 128) - uint256(-amount1);
        return int256((hi << 128) | lo);
    }

    /// Regression test for the review finding: `_v4Settle` negated the
    /// already-positive `outDelta` before converting to `uint128`, producing
    /// the two's-complement of a huge number and making `take` request an
    /// impossible payout. The positive output amount must be passed exactly.
    function testV4SettleTakesExactPositiveOutputZeroForOne() public {
        MintableToken a = new MintableToken();
        MintableToken b = new MintableToken();
        MockPoolManager mgr = new MockPoolManager();
        FlashArbitrageHarness arb = new FlashArbitrageHarness(MOCK_MORPHO);
        mgr.setArbitrage(address(arb));

        // currency0 is whichever token address sorts lower.
        (address currency0, address currency1) =
            address(a) < address(b) ? (address(a), address(b)) : (address(b), address(a));
        MintableToken inTok = MintableToken(currency0);

        // Fund the contract with the input currency so sync/transfer/settle
        // can clear the negative input delta (1,000,000 in, 990,000 out).
        inTok.mint(address(arb), 1_000_000);

        IPoolManagerLike.PoolKey memory key = poolKey(currency0, currency1);
        bytes32 pid =
            PoolKeyChecksum.checksum(key.currency0, key.currency1, key.fee, key.tickSpacing, key.hooks);
        FlashArbitrage.SwapLeg memory leg = buildLeg(address(mgr), true);
        leg.poolId = pid;
        leg.minOut = 500_000;

        int256 amount0 = -int256(1_000_000); // input, currency0
        int256 amount1 = int256(990_000); // output, currency1
        mgr.setSwapDelta(packZ1(amount0, amount1));

        uint256 amountOut = arb.harnessSwap(leg, currency0, currency1, 1_000_000);

        assert(amountOut == 990_000);
        assert(mgr.lastTakeCurrency() == currency1);
        assert(mgr.lastTakeAmount() == 990_000);
        assert(mgr.lastTakeTo() == address(arb));
        assert(mgr.lastZeroForOne());
        // The exact-input amountSpecified must be negative.
        assert(mgr.lastAmountSpecified() == -int256(1_000_000));
    }

    /// Same assertion for the zeroForOne=false direction (currency1 input),
    /// where the delta packing is swapped: amount0 is output (upper bits).
    function testV4SettleTakesExactPositiveOutputZeroForOneFalse() public {
        MintableToken a = new MintableToken();
        MintableToken b = new MintableToken();
        MockPoolManager mgr = new MockPoolManager();
        FlashArbitrageHarness arb = new FlashArbitrageHarness(MOCK_MORPHO);
        mgr.setArbitrage(address(arb));

        (address currency0, address currency1) =
            address(a) < address(b) ? (address(a), address(b)) : (address(b), address(a));
        MintableToken inTok = MintableToken(currency1);

        inTok.mint(address(arb), 2_000_000);

        IPoolManagerLike.PoolKey memory key = poolKey(currency0, currency1);
        bytes32 pid =
            PoolKeyChecksum.checksum(key.currency0, key.currency1, key.fee, key.tickSpacing, key.hooks);
        FlashArbitrage.SwapLeg memory leg = buildLeg(address(mgr), false);
        leg.poolId = pid;
        leg.minOut = 1_000_000;

        // zeroForOne=false: currency1 is the input (negative, lower 128 bits),
        // currency0 the output (positive, upper 128 bits).
        int256 amount1 = -int256(2_000_000);
        int256 amount0 = int256(1_990_000);
        mgr.setSwapDelta(packZ0(amount0, amount1));

        uint256 amountOut = arb.harnessSwap(leg, currency1, currency0, 2_000_000);

        assert(amountOut == 1_990_000);
        assert(mgr.lastTakeCurrency() == currency0);
        assert(mgr.lastTakeAmount() == 1_990_000);
        assert(mgr.lastTakeTo() == address(arb));
        assert(!mgr.lastZeroForOne());
        assert(mgr.lastAmountSpecified() == -int256(2_000_000));
    }

    /// Regression test for the unchecked-transfer finding. `_v4Settle` used a
    /// bare `IERC20.transfer` whose returned bool was discarded, so an input
    /// token that fails without reverting produced a swap that *looked*
    /// settled while the manager's debt went unpaid. With `_safeTransfer` the
    /// failure is raised at the transfer itself, as `TransferFailed`.
    function testV4SettleRejectsFalseReturningInputTransfer() public {
        FalseReturnToken a = new FalseReturnToken();
        FalseReturnToken b = new FalseReturnToken();
        MockPoolManager mgr = new MockPoolManager();
        FlashArbitrageHarness arb = new FlashArbitrageHarness(MOCK_MORPHO);
        mgr.setArbitrage(address(arb));

        // currency0 is whoever sorts lower; the input currency is the token
        // whose transfer lies about succeeding.
        (address currency0, address currency1) =
            address(a) < address(b) ? (address(a), address(b)) : (address(b), address(a));
        FalseReturnToken inTok = FalseReturnToken(currency0);
        inTok.mint(address(arb), 1_000_000);

        IPoolManagerLike.PoolKey memory key = poolKey(currency0, currency1);
        bytes32 pid =
            PoolKeyChecksum.checksum(key.currency0, key.currency1, key.fee, key.tickSpacing, key.hooks);
        FlashArbitrage.SwapLeg memory leg = buildLeg(address(mgr), true);
        leg.poolId = pid;

        mgr.setSwapDelta(packZ1(-int256(1_000_000), int256(990_000)));

        vm.expectRevert(
            abi.encodeWithSignature("TransferFailed(address,address)", currency0, address(mgr))
        );
        arb.harnessSwap(leg, currency0, currency1, 1_000_000);
    }

    // --- Native ETH currency (address(0)) ---
    //
    // Regression tests for the review finding: native V4 legs could not run,
    // because the contract had no payable receiver for the native `take` and
    // `_v4Settle` settled every input as an ERC20 (`IERC20(address(0))`
    // transfer plus a zero-value `settle()`). address(0) always sorts first,
    // so a native-currency pool makes native the currency0:
    //   zeroForOne = true  -> native in, token out
    //   zeroForOne = false -> token in, native out

    /// Native input settled with call value. The manager must receive exactly
    /// the input the swap consumed, and must not see an ERC20 settlement.
    function testV4SettleSendsNativeInputAsCallValue() public {
        // address(0) always sorts first, so a native-currency pool makes the
        // native coin currency0 and any non-zero address currency1.
        address native = address(0);
        address tokenAddr = address(0xBEEF);
        MockPoolManager mgr = new MockPoolManager();
        FlashArbitrageHarness arb = new FlashArbitrageHarness(MOCK_MORPHO);
        mgr.setArbitrage(address(arb));
        assert(native < tokenAddr);

        IPoolManagerLike.PoolKey memory key = IPoolManagerLike.PoolKey({
            currency0: native,
            currency1: tokenAddr,
            fee: 3000,
            tickSpacing: 60,
            hooks: address(0)
        });
        bytes32 pid =
            PoolKeyChecksum.checksum(key.currency0, key.currency1, key.fee, key.tickSpacing, key.hooks);
        FlashArbitrage.SwapLeg memory leg = buildLeg(address(mgr), true);
        leg.poolId = pid;
        leg.minOut = 500_000;

        // zeroForOne: amount0 is the native input (negative), amount1 the
        // token output (positive).
        mgr.setSwapDelta(packZ1(-int256(1_000_000), int256(990_000)));
        vm.deal(address(arb), 1_000_000);

        uint256 amountOut = arb.harnessSwap(leg, native, tokenAddr, 1_000_000);

        assert(amountOut == 990_000);
        assert(mgr.lastTakeCurrency() == tokenAddr);
        assert(mgr.lastTakeAmount() == 990_000);
        assert(mgr.lastSettleValue() == 1_000_000);
        assert(!mgr.lastSettleWasErc20());
        // No ERC20 sync for a native currency, and no ETH left stranded.
        assert(mgr.lastSyncCurrency() == address(0));
        assert(address(arb).balance == 0);
    }

    /// Native output arrives through the PoolManager's ETH transfer, which
    /// requires a payable receiver on the arbitrage contract.
    function testV4SettleReceivesNativeOutput() public {
        MintableToken token = new MintableToken();
        address native = address(0);
        address tokenAddr = address(token);
        MockPoolManager mgr = new MockPoolManager();
        FlashArbitrageHarness arb = new FlashArbitrageHarness(MOCK_MORPHO);
        mgr.setArbitrage(address(arb));

        // tokenAddr sorts above address(0), so the pool ordering holds.
        assert(native < tokenAddr);
        token.mint(address(arb), 2_000_000);
        vm.deal(address(mgr), 3_000_000);

        IPoolManagerLike.PoolKey memory key = IPoolManagerLike.PoolKey({
            currency0: native,
            currency1: tokenAddr,
            fee: 3000,
            tickSpacing: 60,
            hooks: address(0)
        });
        bytes32 pid =
            PoolKeyChecksum.checksum(key.currency0, key.currency1, key.fee, key.tickSpacing, key.hooks);
        FlashArbitrage.SwapLeg memory leg = buildLeg(address(mgr), false);
        leg.poolId = pid;
        leg.minOut = 1_000_000;

        // zeroForOne=false: currency1 (token) is the input (negative, lower
        // bits), currency0 (native) the output (positive, upper bits).
        mgr.setSwapDelta(packZ0(int256(1_990_000), -int256(2_000_000)));

        uint256 amountOut = arb.harnessSwap(leg, tokenAddr, native, 2_000_000);

        assert(amountOut == 1_990_000);
        assert(mgr.lastTakeCurrency() == native);
        assert(mgr.lastTakeTo() == address(arb));
        assert(mgr.nativeTaken() == 1_990_000);
        // The ETH actually landed on the contract (the payable receiver ran).
        assert(address(arb).balance == 1_990_000);
        // The ERC20 input leg still settles through sync + transfer.
        assert(mgr.lastSettleValue() == 0);
        assert(mgr.lastSettleWasErc20());
        assert(mgr.lastSyncCurrency() == tokenAddr);
    }
}

/// ERC20 whose `transfer` returns `false` without reverting and moves nothing.
contract FalseReturnToken {
    mapping(address => uint256) public balanceOf;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function transfer(address, uint256) external pure returns (bool) {
        return false;
    }
}
