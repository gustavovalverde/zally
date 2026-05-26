//! Regression: `Wallet::sync` retries `chain.safe_chain_tip()` for retryable failures.
//!
//! Retries continue until the call succeeds or the policy ceiling is reached. Operator-
//! action and not-retryable failures surface immediately without burning the retry budget.

use zally_chain::ChainSourceError;
use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zally_wallet::{RetryPolicy, WalletError};

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn sync_retries_until_chain_tip_recovers() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::linear(4, 1));

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain
        .handle()
        .fail_safe_chain_tip_next(2, || ChainSourceError::Unavailable {
            reason: "simulated upstream stall".into(),
        });

    let outcome = wallet.sync(&chain).await?;
    assert_eq!(outcome.scanned_to_height.as_u32(), 50);
    assert_eq!(
        chain.handle().failures_consumed(),
        2,
        "mock must have absorbed the two injected failures before succeeding"
    );
    Ok(())
}

#[tokio::test]
async fn sync_does_not_retry_operator_action_chain_failures() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::linear(4, 1));

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain
        .handle()
        .fail_safe_chain_tip_next(1, || ChainSourceError::MalformedCompactBlock {
            block_height: BlockHeight::from(10),
            reason: "synthetic decode failure".into(),
        });

    let outcome = wallet.sync(&chain).await;
    assert!(
        matches!(outcome, Err(WalletError::ChainSource(_))),
        "operator-action error must surface, got {outcome:?}"
    );
    assert_eq!(
        chain.handle().failures_consumed(),
        1,
        "operator-action failures must consume one and only one queued error"
    );
    Ok(())
}
