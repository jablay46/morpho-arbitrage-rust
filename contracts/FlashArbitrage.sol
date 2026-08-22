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

/**
 * @title FlashArbitrage
 * @notice Executes a two-DEX cycle funded by a Morpho Blue flash loan.
 *         Morpho Blue flash loans are fee-free; the loan is repaid by
 *         approving Morpho to pull `assets` back inside the callback.
 */
contract FlashArbitrage {
    struct ArbParams {
        address token;
        uint256 amount;
        address routerA;
        address routerB;
        address[] pathA;
        address[] pathB;
        uint256 minProfit;
    }

    address public immutable morpho;
    address public owner;

    error NotOwner();
    error NotMorpho();
    error Unprofitable(uint256 profit, uint256 minProfit);

    constructor(address _morpho) {
        morpho = _morpho;
        owner = msg.sender;
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    /// Called by the Rust bot. Decodes params and starts the flash loan.
    function execute(ArbParams calldata params) external onlyOwner {
        IMorphoBlue(morpho).flashLoan(params.token, params.amount, abi.encode(params));
    }

    /// Morpho Blue flash loan callback; Morpho calls this on the borrower.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        if (msg.sender != morpho) revert NotMorpho();
        ArbParams memory params = abi.decode(data, (ArbParams));

        uint256 balBefore = IERC20(params.token).balanceOf(address(this));

        // Leg 1: loan token -> quote token on routerA.
        _approve(params.token, params.routerA, assets);
        uint256[] memory amounts1 = IUniswapV2Router(params.routerA).swapExactTokensForTokens(
            assets,
            0,
            params.pathA,
            address(this),
            block.timestamp
        );
        uint256 lastOut = amounts1[amounts1.length - 1];

        // Leg 2: quote token -> loan token on routerB.
        address quoteToken = params.pathA[params.pathA.length - 1];
        _approve(quoteToken, params.routerB, lastOut);
        IUniswapV2Router(params.routerB).swapExactTokensForTokens(
            lastOut,
            0,
            params.pathB,
            address(this),
            block.timestamp
        );

        uint256 balAfter = IERC20(params.token).balanceOf(address(this));
        uint256 profit = balAfter - balBefore;
        if (profit < params.minProfit) revert Unprofitable(profit, params.minProfit);

        // Repay: Morpho pulls `assets` back via transferFrom.
        IERC20(params.token).approve(morpho, assets);

        // Sweep profit to owner.
        if (profit > 0) {
            IERC20(params.token).transfer(owner, profit);
        }
    }

    function _approve(address token, address spender, uint256 amount) internal {
        IERC20(token).approve(spender, amount);
    }
}
