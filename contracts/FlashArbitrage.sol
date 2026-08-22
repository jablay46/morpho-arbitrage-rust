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

interface IUniswapV4PoolManager {
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    function unlock(bytes calldata data) external returns (bytes memory);
    function swap(PoolKey memory key, int128 amountSpecified, bool zeroForOne)
        external
        returns (int128 amount0, int128 amount1);
}

/**
 * @title FlashArbitrage
 * @notice Executes a two-DEX cycle funded by a Morpho Blue flash loan.
 *         Morpho Blue flash loans are fee-free; the loan is repaid by
 *         approving Morpho to pull `assets` back inside the callback.
 *         Supports Uniswap-V2-style routers and Aerodrome-style routers.
 */
contract FlashArbitrage {
    uint8 internal constant KIND_UNISWAP_V2 = 0;
    uint8 internal constant KIND_AERODROME = 1;
    uint8 internal constant KIND_UNISWAP_V3 = 2;
    uint8 internal constant KIND_UNISWAP_V4 = 3;

    struct SwapLeg {
        address router;
        uint8 kind;      // 0=UniV2, 1=Aero, 2=UniV3, 3=UniV4
        address factory; // Aerodrome pool factory (kind 1 only; zero = default)
        bool stable;     // Aerodrome stable pool flag (kind 1 only)
        uint256 minOut;  // Minimum output; bounds slippage from price drift
                         // between simulation and inclusion (Base has a private
                         // sequencer mempool, so no sandwiching; the final
                         // profit check is the backstop).
        uint24 feeTier;  // Uniswap V3 fee tier (kind 2 only)
        bytes32 poolId;  // Uniswap V4 pool ID (kind 3 only)
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
        _approve(from, leg.router, amountIn);
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
        if (leg.kind == KIND_UNISWAP_V4) {
            // V4 uses an unlock/lock pattern that doesn't fit this flashloan
            // arbitrage model; reserve for future implementation.
            revert UnknownLegKind(leg.kind);
        }
        revert UnknownLegKind(leg.kind);
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
