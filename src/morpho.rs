use alloy::sol;

sol! {
    /// Morpho Blue flash loan entry point.
    /// `flashLoan` transfers `assets` of `token` to the caller and expects the
    /// caller's callback `onMorphoFlashLoan(uint256,bytes)` to succeed and the
    /// full amount to be approved back for pull within the same tx.
    #[sol(rpc)]
    interface IMorphoBlue {
        function flashLoan(address token, uint256 assets, bytes data) external;
    }
}


