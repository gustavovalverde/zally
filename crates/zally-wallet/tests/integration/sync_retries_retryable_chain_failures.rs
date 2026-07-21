//! Regression: `Wallet::sync` retries complete chain-epoch attempts for retryable failures.
//!
//! Retries continue until the call succeeds or the policy ceiling is reached. Operator-
//! action and not-retryable failures surface immediately without burning the retry budget.

use zally_chain::ChainSourceError;
use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zally_wallet::{RetryPolicy, WalletError};

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn sync_retries_until_current_epoch_recovers() -> Result<(), TestWalletError> {
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
        .fail_current_epoch_next(2, || ChainSourceError::Unavailable {
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
async fn missing_capabilities_fail_before_wallet_state_mutation() -> Result<(), TestWalletError> {
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
        .fail_current_epoch_next(1, || ChainSourceError::CapabilitiesUnavailable {
            capabilities: vec!["wallet.compact_block.range.v1".to_owned()],
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
    let status = wallet.status_snapshot().await?;
    assert_eq!(status.scanned_height, None);
    assert_eq!(status.settled_tip_height, None);
    Ok(())
}

#[tokio::test]
async fn stale_epoch_restarts_every_artifact_read_with_a_fresh_epoch() -> Result<(), TestWalletError>
{
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    wallet.set_retry_policy(RetryPolicy::linear(3, 1));

    let chain = MockChainSource::new(wallet.network());
    let handle = chain.handle();
    handle.advance_tip(BlockHeight::from(50));
    handle.expire_epoch_on_next_compact_read();

    let outcome = wallet.sync(&chain).await?;
    assert_eq!(outcome.scanned_to_height, BlockHeight::from(50));
    assert_eq!(handle.acquired_epoch_ids(), vec![1, 2]);
    let artifact_epochs = handle.artifact_epoch_ids();
    assert_eq!(artifact_epochs.get(..2), Some([1, 1].as_slice()));
    assert!(
        artifact_epochs.iter().skip(2).all(|epoch| *epoch == 2),
        "artifact reads after the stale pin must use only epoch 2: {artifact_epochs:?}"
    );
    Ok(())
}
