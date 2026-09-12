// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IMorphoBlue {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

interface IERC20 {
    function balanceOf(address account) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function allowance(address owner, address spender) external view returns (uint256);
}

interface IUniswapV2Router {
    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory amounts);
}

interface IAerodromeRouter {
    struct Route {
        address from;
        address to;
        bool stable;
        address factory;
    }

    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        Route[] calldata routes,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory amounts);
}

interface IUniswapV3Router {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams calldata params)
        external
        returns (uint256 amountOut);
}

interface ISlipstreamRouter {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        int24 tickSpacing;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams calldata params)
        external
        returns (uint256 amountOut);
}

interface IUniswapV4PoolManager {
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

/// PoolId helper matching Uniswap v4's canonical `PoolIdLibrary.toId`:
/// keccak256(abi.encode(PoolKey)).
library PoolIdLibrary {
    function toId(IUniswapV4PoolManager.PoolKey memory key) internal pure returns (bytes32) {
        return keccak256(abi.encode(key));
    }
}

/**
 * @title FlashArbitrage
 * @notice Executes a two-DEX cycle funded by a Morpho Blue flash loan.
 *         Morpho Blue flash loans are fee-free; the loan is repaid by
 *         approving Morpho to pull `assets` back inside the callback.
 *         Supports Uniswap-V2-style, Aerodrome vAMM, Uniswap-V3-style,
 *         Aerodrome Slipstream (CL), and Uniswap V4 routers.
 */
contract FlashArbitrage {
    uint8 internal constant KIND_UNISWAP_V2 = 0;
    uint8 internal constant KIND_AERODROME = 1;
    uint8 internal constant KIND_UNISWAP_V3 = 2;
    uint8 internal constant KIND_UNISWAP_V4 = 3;
    uint8 internal constant KIND_SLIPSTREAM = 4;

    struct SwapLeg {
        address router;
        uint8 kind;      // 0=UniV2, 1=Aero, 2=UniV3, 3=UniV4
        address factory; // Aerodrome pool factory (kind 1 only; zero = default)
        bool stable;     // Aerodrome stable pool flag (kind 1 only)
        uint256 minOut;  // Minimum output; bounds slippage from price drift
                         // between simulation and inclusion (Base has a private
                         // sequencer mempool, so no sandwiching; the final
                         // profit check is the backstop).
        uint24 feeTier;  // Uniswap V3 fee tier (kind 2 only) / Uniswap V4
                         // pool fee in hundredths of a bip (kind 3 only; same
                         // unit as V3, passed straight through from config).
                         // Dynamic-fee pools (fee == 0x800000) are unsupported:
                         // they need the ERC1155 fee-token settlement, not a
                         // plain IERC20 approve.
        bytes32 poolId;  // Uniswap V4 pool ID (kind 3 only) = keccak256 of the
                         // ABI-encoded PoolKey; verified against a locally
                         // reconstructed key before the swap.
        int24 tickSpacing; // Uniswap V4 tick spacing (kind 3 only)
        address hooks;  // Uniswap V4 hooks address (kind 3 only; zero = none)
        bool zeroForOne;  // Uniswap V4 swap direction (kind 3 only: true =
                         // currency0 -> currency1). Determined off-chain by
                         // matching keccak256(abi.encode(PoolKey)) to poolId.
    }

    struct ArbParams {
        address token;
        address quote;
        uint256 amount;
        SwapLeg legA; // token -> quote
        SwapLeg legB; // quote -> token
        uint256 minProfit;
    }

    address public immutable morpho;
    address public owner;

    error NotOwner();
    error NotMorpho();
    error UnknownLegKind(uint8 kind);
    error Unprofitable(uint256 profit, uint256 minProfit);
    error ApproveFailed(address token, address spender);
    error TransferFailed(address token, address to);
    error V4InputMismatch(address currency, bytes32 poolId);
    error V4PoolIdMismatch(bytes32 expected, bytes32 actual);
    error V4SwapDeltaMismatch();
    error V4MinOutput(uint256 out, uint256 minOut);
    error V4AmountTooLarge(uint256 amountIn);

    /// Canonical TickMath bounds, identical for Uniswap V3 and V4. Used as the
    /// unrestricted sqrt price limit in `unlockCallback` (the exact values the
    /// V4Quoter/UniversalRouter pass to `poolManager.swap`).
    uint160 internal constant V4_MIN_SQRT_PRICE = 4295128739; // getSqrtRatioAtTick(MIN_TICK)
    uint160 internal constant V4_MAX_SQRT_PRICE =
        1461446703485210103287273052203988822378723970342; // getSqrtRatioAtTick(MAX_TICK)

    constructor(address _morpho) {
        morpho = _morpho;
        owner = msg.sender;
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    /// Called by the Rust bot. Starts the flash loan with the encoded params.
    function execute(ArbParams calldata params) external onlyOwner {
        IMorphoBlue(morpho).flashLoan(params.token, params.amount, abi.encode(params));
    }

    /// Morpho Blue flash loan callback; only Morpho may call this.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        if (msg.sender != morpho) revert NotMorpho();
        ArbParams memory params = abi.decode(data, (ArbParams));

        uint256 balBefore = IERC20(params.token).balanceOf(address(this));

        // Leg 1: loan token -> quote token.
        uint256 quoteOut = _swap(params.legA, params.token, params.quote, assets);
        // Leg 2: quote token -> loan token.
        _swap(params.legB, params.quote, params.token, quoteOut);

        uint256 balAfter = IERC20(params.token).balanceOf(address(this));
        uint256 profit = balAfter - balBefore;
        if (profit < params.minProfit) revert Unprofitable(profit, params.minProfit);

        // Repay: Morpho pulls `assets` back via transferFrom after the callback.
        _approve(params.token, morpho, assets);

        // Sweep profit to owner. Balance left is balBefore >= assets, so the
        // subsequent Morpho pull still succeeds.
        if (profit > 0) {
            _safeTransfer(params.token, owner, profit);
        }
    }

    /// Rescue any token stuck in this contract (dust, failed runs).
    function sweep(address token) external onlyOwner {
        _safeTransfer(token, owner, IERC20(token).balanceOf(address(this)));
    }

    function _swap(SwapLeg memory leg, address from, address to, uint256 amountIn)
        internal
        returns (uint256 amountOut)
    {
        // Routers pull the input via transferFrom, so approve the router for
        // every leg EXCEPT V4: the PoolManager's settle() path transfers the
        // input directly (no allowance needed), so an approve there would be
        // a wasted SSTORE and an unnecessary external surface.
        if (leg.kind != KIND_UNISWAP_V4) {
            _approve(from, leg.router, amountIn);
        }
        if (leg.kind == KIND_UNISWAP_V2) {
            address[] memory path = new address[](2);
            path[0] = from;
            path[1] = to;
            uint256[] memory amounts = IUniswapV2Router(leg.router).swapExactTokensForTokens(
                amountIn, leg.minOut, path, address(this), block.timestamp
            );
            return amounts[amounts.length - 1];
        }
        if (leg.kind == KIND_AERODROME) {
            IAerodromeRouter.Route[] memory routes = new IAerodromeRouter.Route[](1);
            routes[0] = IAerodromeRouter.Route(from, to, leg.stable, leg.factory);
            uint256[] memory amounts = IAerodromeRouter(leg.router).swapExactTokensForTokens(
                amountIn, leg.minOut, routes, address(this), block.timestamp
            );
            return amounts[amounts.length - 1];
        }
        if (leg.kind == KIND_UNISWAP_V3) {
            IUniswapV3Router.ExactInputSingleParams memory params = IUniswapV3Router.ExactInputSingleParams({
                tokenIn: from,
                tokenOut: to,
                fee: leg.feeTier,
                recipient: address(this),
                deadline: block.timestamp,
                amountIn: amountIn,
                amountOutMinimum: leg.minOut,
                sqrtPriceLimitX96: 0
            });
            return IUniswapV3Router(leg.router).exactInputSingle(params);
        }
        if (leg.kind == KIND_SLIPSTREAM) {
            // Aerodrome Slipstream (CL) router: structurally identical to V3
            // but the pool discriminator is int24 tickSpacing, not uint24 fee
            // (selector 0xa026383e, not 0x414bf389).
            ISlipstreamRouter.ExactInputSingleParams memory params = ISlipstreamRouter.ExactInputSingleParams({
                tokenIn: from,
                tokenOut: to,
                // feeTier holds the Slipstream tickSpacing (1..2000, all
                // positive); widen via uint256 then narrow through int256.
                tickSpacing: int24(int256(uint256(leg.feeTier))),
                recipient: address(this),
                deadline: block.timestamp,
                amountIn: amountIn,
                amountOutMinimum: leg.minOut,
                sqrtPriceLimitX96: 0
            });
            return ISlipstreamRouter(leg.router).exactInputSingle(params);
        }
        if (leg.kind == KIND_UNISWAP_V4) {
            return _swapV4(leg, from, to, amountIn);
        }
    }

    /// Uniswap V4 swap through the singleton PoolManager's unlock/lock pattern:
    /// unlock the manager, swap inside the callback, settle the input debt
    /// (sync -> transfer -> settle) and take the output inside the callback,
    /// then let the manager re-lock.
    ///
    /// Dynamic-fee pools (fee == 0x800000) are unsupported: they groom
    /// currency0/1 from the router and need ERC1155 fee-token settlement, not
    /// a plain IERC20 approve. Hooks that alter the swap delta are handled in
    /// `unlockCallback` by decoding the returned delta and settling the
    /// *actual* input consumed.
    function _swapV4(SwapLeg memory leg, address from, address to, uint256 amountIn)
        internal
        returns (uint256 amountOut)
    {
        IUniswapV4PoolManager manager = IUniswapV4PoolManager(leg.router);
        _v4LastOut = 0;
        // The callback reconstructs the PoolKey from `from`/`to`/leg; passing
        // the whole `leg` (instead of ten scalars) keeps the call frame under
        // the legacy stack limit.
        manager.unlock(abi.encode(leg, from, to, amountIn));
        // unlock re-locked the manager and netted every delta to zero; the
        // callback stored the actual output in `_v4LastOut`. Any revert inside
        // unwinds the whole outer transaction.
        amountOut = _v4LastOut;
        _v4LastOut = 0;
        if (amountOut < leg.minOut) revert V4MinOutput(amountOut, leg.minOut);
    }

    /// Last V4 swap output, set by `unlockCallback` and consumed (then
    /// cleared) by `_swapV4`. Only meaningful between the `unlock` call and
    /// the re-lock for that leg; a leftover from a reverted unlock is reset
    /// by `_swapV4` before unlocking again.
    uint256 private _v4LastOut;

    /// The PoolManager unlock callback; executes the V4 swap plus settlement.
    /// Guarded: only the PoolManager may call this (msg.sender check) and the
    /// data must be well-formed (otherwise a griefing caller could force
    /// arbitrary `take` recipients via a crafted call, since the manager calls
    /// ANY address's unlockCallback after unlock()). The swap/accounting is
    /// otherwise self-contained inside the callback.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        (SwapLeg memory leg, address from, address to, uint256 amountIn) =
            abi.decode(data, (SwapLeg, address, address, uint256));
        // `leg.router` is the PoolManager; only it may enter the callback.
        if (msg.sender != leg.router) revert V4InputMismatch(msg.sender, leg.poolId);
        if (from == to) revert V4InputMismatch(from, leg.poolId);
        _v4LastOut = _v4Settle(leg, from, to, amountIn);
        return "";
    }

    /// Core V4 swap + settlement inside the unlock callback. Reverts on any
    /// misconfiguration (bad poolId, wrong direction) or malformed pool
    /// response; any revert unwinds `unlock` entirely.
    ///
    /// The PoolKey is NOT stored on-chain (Uniswap V4 explicitly says
    /// "poolkeys are not saved in storage and must always be provided by the
    /// caller"). Reconstruct it from the leg fields and verify it against the
    /// poolId in the config; this replaces the non-existent getPoolKey(poolId)
    /// getter and catches misconfigured legs.
    function _v4Settle(
        SwapLeg memory leg,
        address from,
        address to,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        address manager = leg.router;
        bytes32 poolId = leg.poolId;
        bool zeroForOne = leg.zeroForOne;
        IUniswapV4PoolManager.PoolKey memory key = IUniswapV4PoolManager.PoolKey({
            currency0: from < to ? from : to,
            currency1: from < to ? to : from,
            fee: leg.feeTier,
            tickSpacing: leg.tickSpacing,
            hooks: leg.hooks
        });
        bytes32 derivedId = PoolIdLibrary.toId(key);
        if (derivedId != poolId) revert V4PoolIdMismatch(poolId, derivedId);
        // `from` is the input token and must be the currency being sold:
        // currency0 when zeroForOne, currency1 otherwise.
        if (zeroForOne != (from == key.currency0)) revert V4InputMismatch(from, poolId);
        if (to != (zeroForOne ? key.currency1 : key.currency0)) revert V4InputMismatch(to, poolId);

        // Explicit guard: -int256(amountIn) would overflow when amountIn
        // exceeds int256::max (uint256 is wider).
        if (amountIn > uint256(type(int256).max)) revert V4AmountTooLarge(amountIn);
        IUniswapV4PoolManager.SwapParams memory params = IUniswapV4PoolManager.SwapParams({
            zeroForOne: zeroForOne,
            amountSpecified: -int256(amountIn), // negative = exact input
            sqrtPriceLimitX96: zeroForOne ? V4_MIN_SQRT_PRICE + 1 : V4_MAX_SQRT_PRICE - 1
        });
        int256 rawDelta = IUniswapV4PoolManager(manager).swap(key, params, new bytes(0));
        // BalanceDelta: upper 128 bits = amount0, lower 128 = amount1.
        // Exact-input swaps have the input side negative and the output side
        // positive (e.g. zeroForOne: amount0 < 0 = input, amount1 > 0 = output).
        int128 inDelta = zeroForOne ? int128(rawDelta >> 128) : int128(rawDelta);
        int128 outDelta = zeroForOne ? int128(rawDelta) : int128(rawDelta >> 128);
        if (inDelta >= 0 || outDelta <= 0) revert V4SwapDeltaMismatch();
        uint256 actualIn = uint256(uint128(-inDelta));
        amountOut = uint256(uint128(-outDelta));
        if (amountOut < leg.minOut) revert V4MinOutput(amountOut, leg.minOut);

        // Withdraw the output to this contract.
        IUniswapV4PoolManager(manager).take(
            zeroForOne ? key.currency1 : key.currency0,
            address(this),
            amountOut
        );
        // Settle the input debt: sync snapshots the manager's balance, then
        // transfer the EXACT input consumed (from the delta, not amountIn — a
        // price limit or hook may have consumed less) and settle.
        IUniswapV4PoolManager(manager).sync(zeroForOne ? key.currency0 : key.currency1);
        IERC20(zeroForOne ? key.currency0 : key.currency1).transfer(manager, actualIn);
        IUniswapV4PoolManager(manager).settle();
    }

    function _approve(address token, address spender, uint256 amount) internal {
        // Only reset if the current allowance is non-zero but insufficient;
        // this avoids a wasted SSTORE for fresh/adequate allowances while
        // still handling non-standard ERC20s (e.g. USDT-style) that reject
        // changing a non-zero allowance directly.
        uint256 current = IERC20(token).allowance(address(this), spender);
        if (current != 0 && current < amount) {
            _safeApprove(token, spender, 0);
        }
        if (current < amount) {
            _safeApprove(token, spender, amount);
        }
    }

    function _safeApprove(address token, address spender, uint256 amount) private {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.approve.selector, spender, amount));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) {
            revert ApproveFailed(token, spender);
        }
    }

    // Same non-standard-token handling as _safeApprove: tolerate tokens that
    // return no data (USDT-style) and require `true` when data is returned.
    function _safeTransfer(address token, address to, uint256 amount) private {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.transfer.selector, to, amount));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) {
            revert TransferFailed(token, to);
        }
    }
}
