// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {FlashArbitrage} from "../contracts/FlashArbitrage.sol";

/// Minimal cheatcode interface (no forge-std dependency).
interface Vm {
    struct Log {
        bytes32[] topics;
        bytes data;
        address emitter;
    }

    function addr(uint256) external returns (address);
    function prank(address) external;
    function expectRevert(bytes calldata) external;
    function recordLogs() external;
    function getRecordedLogs() external returns (Log[] memory);
}

interface IERC20Like {
    function balanceOf(address account) external view returns (uint256);
    function allowance(address owner, address spender) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

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

/// V2-style router that pays a fixed `output` regardless of the input path,
/// so the test can drive a full two-leg cycle through `execute`.
contract MockV2Router {
    uint256 public output;

    function setOutput(uint256 o) external {
        output = o;
    }

    function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] calldata path, address to, uint256)
        external
        returns (uint256[] memory amounts)
    {
        require(output >= amountOutMin, "slippage");
        IERC20Like(path[0]).transferFrom(msg.sender, address(this), amountIn);
        IERC20Like(path[1]).transfer(to, output);
        amounts = new uint256[](2);
        amounts[0] = amountIn;
        amounts[1] = output;
    }
}

/// Slipstream-style router that records the tickSpacing it was given.
contract MockSlipstreamRouter {
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

    int24 public lastTickSpacing;
    uint256 public lastAmountIn;

    function exactInputSingle(ExactInputSingleParams calldata p) external returns (uint256) {
        lastTickSpacing = p.tickSpacing;
        lastAmountIn = p.amountIn;
        return p.amountIn;
    }
}

/// Mock Morpho Blue: funds the flash loan, synchronously enters the callback
/// (exactly like the real protocol), then pulls the loan back via the
/// approval the contract sets inside the callback.
contract MockMorpho {
    FlashArbitrage public arb;

    function setArb(address a) external {
        arb = FlashArbitrage(a);
    }

    function flashLoan(address token, uint256 assets, bytes calldata data) external {
        IERC20Like(token).transfer(address(arb), assets);
        arb.onMorphoFlashLoan(assets, data);
        IERC20Like(token).transferFrom(address(arb), address(this), assets);
    }
}

/// Exposes the internal `_swap` so Slipstream bounds behaviour can be tested
/// without the full Morpho flash-loan flow.
contract FlashArbitrageHarness is FlashArbitrage {
    constructor(address _morpho) FlashArbitrage(_morpho) {}

    function harnessSwap(
        FlashArbitrage.SwapLeg calldata leg,
        address from,
        address to,
        uint256 amountIn
    ) external returns (uint256) {
        return _swap(leg, from, to, amountIn);
    }
}

contract HardeningTest {
    Vm constant vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);

    address constant MOCK_MORPHO = address(0x0000000000000000000000000000000000000a11);

    function buildLeg(uint8 kind, address router) internal pure returns (FlashArbitrage.SwapLeg memory leg) {
        leg = FlashArbitrage.SwapLeg({
            router: router,
            kind: kind,
            factory: address(0),
            stable: false,
            minOut: 0,
            feeTier: 0,
            poolId: bytes32(0),
            tickSpacing: 0,
            hooks: address(0),
            zeroForOne: false
        });
    }

    // --- Ownership transfer (two-step) ---

    function testTwoStepOwnershipTransfer() public {
        FlashArbitrage arb = new FlashArbitrage(MOCK_MORPHO);
        address alice = vm.addr(0xaaaa);
        address bob = vm.addr(0xbbbb);

        assert(arb.owner() == address(this));

        arb.transferOwnership(alice);
        assert(arb.pendingOwner() == alice);
        assert(arb.owner() == address(this));

        // Someone who was never nominated cannot accept.
        vm.prank(bob);
        vm.expectRevert(abi.encodeWithSignature("NotPendingOwner()"));
        arb.acceptOwnership();

        vm.prank(alice);
        arb.acceptOwnership();
        assert(arb.owner() == alice);
        assert(arb.pendingOwner() == address(0));
    }

    function testNonOwnerCannotStartTransfer() public {
        FlashArbitrage arb = new FlashArbitrage(MOCK_MORPHO);

        vm.prank(vm.addr(0x1111));
        vm.expectRevert(abi.encodeWithSignature("NotOwner()"));
        arb.transferOwnership(vm.addr(0x2222));
    }

    function testOwnershipEventsEmitted() public {
        vm.recordLogs();
        FlashArbitrage arb = new FlashArbitrage(MOCK_MORPHO);

        address alice = vm.addr(0xaaaa);
        arb.transferOwnership(alice);
        vm.prank(alice);
        arb.acceptOwnership();

        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 transferStarted = keccak256("OwnershipTransferStarted(address,address)");
        bytes32 transferred = keccak256("OwnershipTransferred(address,address)");
        bool sawStarted;
        bool sawTransferred;
        bool sawConstructorEvent;
        for (uint256 i = 0; i < logs.length; i++) {
            if (logs[i].topics[0] == transferStarted) sawStarted = true;
            if (logs[i].topics[0] == transferred) {
                // constructor emit has prevOwner == 0; acceptOwnership has alice/re-alice.
                if (logs[i].topics[1] == bytes32(uint256(uint160(address(0))))) {
                    sawConstructorEvent = true;
                } else {
                    sawTransferred = true;
                }
            }
        }
        assert(sawStarted);
        assert(sawTransferred);
        assert(sawConstructorEvent);
    }

    // --- Reentrancy guard ---

    function testDirectCallbackWithoutExecuteReverts() public {
        FlashArbitrage arb = new FlashArbitrage(MOCK_MORPHO);

        // Even from the Morpho address, the callback may only be entered while
        // an `execute` holds the lock.
        vm.prank(MOCK_MORPHO);
        vm.expectRevert(abi.encodeWithSignature("Reentrant()"));
        arb.onMorphoFlashLoan(1, "");
    }

    function testCallbackFromNonMorphoReverts() public {
        FlashArbitrage arb = new FlashArbitrage(MOCK_MORPHO);

        vm.expectRevert(abi.encodeWithSignature("NotMorpho()"));
        arb.onMorphoFlashLoan(1, "");
    }

    function testFullCycleEmitsArbExecutedAndProfitSwept() public {
        MintableToken token = new MintableToken();
        MintableToken quote = new MintableToken();
        MockV2Router routerA = new MockV2Router();
        MockV2Router routerB = new MockV2Router();
        MockMorpho morpho = new MockMorpho();
        FlashArbitrage arb = new FlashArbitrage(address(morpho));
        morpho.setArb(address(arb));

        uint256 assets = 1_000_000;

        // legA: token -> quote pays 1,000,000 quote; legB: quote -> token
        // pays 1,001,000 token, leaving 1,000 profit after the loan is repaid.
        routerA.setOutput(1_000_000);
        routerB.setOutput(1_001_000);
        token.mint(address(morpho), assets);
        token.mint(address(routerB), 1_001_000);
        quote.mint(address(routerA), 1_000_000);

        FlashArbitrage.SwapLeg memory legA = buildLeg(0, address(routerA));
        FlashArbitrage.SwapLeg memory legB = buildLeg(0, address(routerB));

        vm.recordLogs();
        arb.execute(
            FlashArbitrage.ArbParams({
                token: address(token),
                quote: address(quote),
                amount: assets,
                legA: legA,
                legB: legB,
                minProfit: 500
            })
        );

        // Loan repaid, profit swept to owner (this test contract).
        assert(token.balanceOf(address(arb)) == 0);
        assert(token.balanceOf(address(this)) == 1_000);

        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 arbExecuted = keccak256("ArbExecuted(address,address,uint256,uint256)");
        bool saw;
        for (uint256 i = 0; i < logs.length; i++) {
            if (logs[i].topics[0] == arbExecuted
                && logs[i].topics[1] == bytes32(uint256(uint160(address(token))))
                && logs[i].topics[2] == bytes32(uint256(uint160(address(quote))))
            ) {
                (uint256 amount, uint256 profit) = abi.decode(logs[i].data, (uint256, uint256));
                assert(amount == assets);
                assert(profit == 1_000);
                saw = true;
            }
        }
        assert(saw);
    }

    function testLockResetsAfterSuccessfulCycle() public {
        // The guard must not stay stuck set after a completed flash-loan run:
        // a second cycle immediately after the first must still execute.
        MintableToken token = new MintableToken();
        MintableToken quote = new MintableToken();
        MockV2Router router = new MockV2Router();
        MockV2Router routerB = new MockV2Router();
        MockMorpho morpho = new MockMorpho();
        FlashArbitrage arb = new FlashArbitrage(address(morpho));
        morpho.setArb(address(arb));

        uint256 assets = 1_000_000;
        router.setOutput(1_000_000);
        routerB.setOutput(1_000_000);
        token.mint(address(morpho), assets);

        FlashArbitrage.SwapLeg memory legA = buildLeg(0, address(router));
        FlashArbitrage.SwapLeg memory legB = buildLeg(0, address(routerB));

        // First cycle ends with zero profit (1_000_000 -> 1_000_000) and no
        // revert (minProfit 0). The routers' quote/token are spent by the
        // cycle, so re-fund them each iteration; the guard must not stay
        // stuck set and the loan balance must always be back at `assets`.
        for (uint256 i = 0; i < 2; i++) {
            quote.mint(address(router), 1_000_000);
            token.mint(address(routerB), 1_000_000);
            arb.execute(
                FlashArbitrage.ArbParams({
                    token: address(token),
                    quote: address(quote),
                    amount: assets,
                    legA: legA,
                    legB: legB,
                    minProfit: 0
                })
            );
            assert(token.balanceOf(address(morpho)) == assets);
            assert(quote.balanceOf(address(router)) == 0);
        }
    }

    // --- Slipstream tickSpacing bounds ---

    function testSlipstreamOutOfRangeReverts() public {
        FlashArbitrageHarness harness = new FlashArbitrageHarness(MOCK_MORPHO);
        MintableToken token = new MintableToken();
        MintableToken quote = new MintableToken();

        FlashArbitrage.SwapLeg memory leg = buildLeg(4, address(0x1111));
        leg.feeTier = 2001; // > MAX_TICK_SPACING (2000)

        vm.expectRevert(abi.encodeWithSignature("TickSpacingOutOfRange(uint24)", uint24(2001)));
        harness.harnessSwap(leg, address(token), address(quote), 1_000);
    }

    function testSlipstreamInRangePassesThrough() public {
        FlashArbitrageHarness harness = new FlashArbitrageHarness(MOCK_MORPHO);
        MockSlipstreamRouter router = new MockSlipstreamRouter();
        MintableToken token = new MintableToken();
        MintableToken quote = new MintableToken();
        // `_swap` approves the router for the input token first; fund the
        // harness so the approval path behaves like a real run.
        token.mint(address(harness), 5_000);

        FlashArbitrage.SwapLeg memory leg = buildLeg(4, address(router));
        leg.feeTier = 200; // within 1..2000, must reach the router

        uint256 out = harness.harnessSwap(leg, address(token), address(quote), 5_000);
        assert(out == 5_000);
        assert(router.lastTickSpacing() == 200);
        assert(router.lastAmountIn() == 5_000);
    }

    // --- Sweep event ---

    function testSweepEmitsEvent() public {
        FlashArbitrage arb = new FlashArbitrage(MOCK_MORPHO);
        MintableToken token = new MintableToken();
        token.mint(address(arb), 1_000);

        vm.recordLogs();
        arb.sweep(address(token));

        assert(token.balanceOf(address(this)) == 1_000);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 sweepTopic = keccak256("Swept(address,address,uint256)");
        bool saw;
        for (uint256 i = 0; i < logs.length; i++) {
            if (logs[i].topics[0] == sweepTopic) saw = true;
        }
        assert(saw);
    }
}