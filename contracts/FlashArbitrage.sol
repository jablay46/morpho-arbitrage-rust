// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IMorphoBlue {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

interface IERC20 {
    function balanceOf(address account) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
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

    struct SwapLeg {
        address router;
        uint8 kind;      // 0 = UniswapV2-style, 1 = Aerodrome-style
        address factory; // Aerodrome pool factory (kind 1 only; zero = default)
        bool stable;     // Aerodrome stable pool flag (kind 1 only)
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
            IERC20(params.token).transfer(owner, profit);
        }
    }

    /// Rescue any token stuck in this contract (dust, failed runs).
    function sweep(address token) external onlyOwner {
        IERC20(token).transfer(owner, IERC20(token).balanceOf(address(this)));
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
                amountIn, 0, path, address(this), block.timestamp
            );
            return amounts[amounts.length - 1];
        }
        if (leg.kind == KIND_AERODROME) {
            IAerodromeRouter.Route[] memory routes = new IAerodromeRouter.Route[](1);
            routes[0] = IAerodromeRouter.Route(from, to, leg.stable, leg.factory);
            uint256[] memory amounts = IAerodromeRouter(leg.router).swapExactTokensForTokens(
                amountIn, 0, routes, address(this), block.timestamp
            );
            return amounts[amounts.length - 1];
        }
        revert UnknownLegKind(leg.kind);
    }

    function _approve(address token, address spender, uint256 amount) internal {
        // Reset first for non-standard ERC20s (e.g. USDT-style) that reject
        // changing a non-zero allowance directly.
        if (!IERC20(token).approve(spender, 0)) revert ApproveFailed(token, spender);
        if (!IERC20(token).approve(spender, amount)) revert ApproveFailed(token, spender);
    }
}
