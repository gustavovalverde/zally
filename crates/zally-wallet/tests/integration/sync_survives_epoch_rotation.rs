//! Regression: a source that rotates its chain epoch under a running sync attempt must not
//! stall the wallet.
//!
//! Zinder retains exactly one epoch and rotates it on every canonical append, so a wallet
//! far enough behind meets [`ChainSourceError::ChainEpochPinUnavailable`] on every attempt
//! whose scan outlives one block. The pin expiry is a restart signal, not backend trouble:
//! the breaker must stay closed, the ladder must not park, and every attempt must keep the
//! blocks it committed before the pin expired.

use std::sync::Arc;

use zally_chain::{ChainSource, ChainSourceError};
use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zally_wallet::{
    CircuitBreakerState, RetryPolicy, SyncDriver, SyncDriverOptions, SyncDriverPhase,
    SyncRecoveryPolicy, WalletError,
};

use super::fixtures::{
    SnapshotWaitError, TestWalletError, TestWalletFixture, capture_sync_events, create_test_wallet,
    wait_for_snapshot,
};

/// Blocks per scan chunk in `Wallet::sync`, so a test can size a catch-up in whole chunks.
const BLOCKS_PER_SYNC_CHUNK: u32 = 1_000;

#[tokio::test]
async fn epoch_rotation_leaves_the_circuit_breaker_closed() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::none());

    let chain = MockChainSource::new(network);
    let chain_handle = chain.handle();
    chain_handle.serve_compact_blocks();
    chain_handle.advance_tip(BlockHeight::from(50));
    chain_handle.fail_transparent_utxos_next(20, || ChainSourceError::ChainEpochPinUnavailable);

    for _ in 0..10 {
        let outcome = wallet.sync(&chain).await;
        assert!(
            matches!(
                outcome,
                Err(WalletError::ChainSource(
                    ChainSourceError::ChainEpochPinUnavailable
                ))
            ),
            "the rotated pin must surface as itself, got {outcome:?}"
        );
        assert!(
            matches!(
                wallet.circuit_breaker_state(),
                CircuitBreakerState::Closed {
                    consecutive_failures: 0
                }
            ),
            "an expired epoch pin must not advance the breaker, got {:?}",
            wallet.circuit_breaker_state()
        );
    }
    assert_eq!(chain_handle.failures_consumed(), 10);
    Ok(())
}

#[tokio::test]
async fn wallet_far_behind_converges_across_repeated_epoch_rotations() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::none());

    let (_capture_guard, sync_events) = capture_sync_events();

    let chunk_count = 8;
    let tip = BlockHeight::from(BLOCKS_PER_SYNC_CHUNK * chunk_count);
    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    chain_handle.serve_compact_blocks();
    chain_handle.advance_tip(tip);
    chain_handle
        .fail_transparent_utxos_next(chunk_count, || ChainSourceError::ChainEpochPinUnavailable);

    let driver = SyncDriver::new(
        wallet.clone(),
        chain as Arc<dyn ChainSource>,
        SyncDriverOptions::default()
            .with_poll_interval_ms(25)
            .with_recovery_policy(
                SyncRecoveryPolicy::default()
                    .with_fault_backoff_initial_ms(20)
                    .with_fault_backoff_cap_ms(40),
            ),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    let caught_up = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting && snapshot.scanned_height == Some(tip)
    })
    .await?;
    assert_eq!(caught_up.last_fault, None);
    assert_eq!(
        chain_handle.failures_consumed(),
        chunk_count,
        "every scan chunk must have met a rotated epoch pin"
    );
    assert!(
        matches!(
            wallet.circuit_breaker_state(),
            CircuitBreakerState::Closed { .. }
        ),
        "catching up across epoch rotations must leave the breaker closed, got {:?}",
        wallet.circuit_breaker_state()
    );

    assert!(
        sync_events.contains("wallet_sync_slow_progress"),
        "each rotated attempt kept its committed blocks and must publish slow progress"
    );
    assert!(
        !sync_events.contains("wallet_sync_fault"),
        "epoch rotation must not strike the repair ladder"
    );
    assert!(
        !sync_events.contains("wallet_sync_parked"),
        "epoch rotation must never park the driver"
    );
    assert!(
        sync_events.contains("reason=chain_epoch_expired"),
        "a catch-up that never reached the tree-root comparison must say so"
    );

    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn rotation_before_any_committed_block_still_reaches_park() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::none());

    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    chain_handle.serve_compact_blocks();
    chain_handle.advance_tip(BlockHeight::from(50));
    for _ in 0..10 {
        chain_handle.expire_epoch_on_next_compact_read();
    }

    let driver = SyncDriver::new(
        wallet.clone(),
        chain as Arc<dyn ChainSource>,
        SyncDriverOptions::default()
            .with_poll_interval_ms(25)
            .with_recovery_policy(
                SyncRecoveryPolicy::default()
                    .with_fault_backoff_initial_ms(20)
                    .with_fault_backoff_cap_ms(40),
            ),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, SyncDriverPhase::Parked { .. })
    })
    .await?;
    assert!(
        matches!(
            wallet.circuit_breaker_state(),
            CircuitBreakerState::Closed { .. }
        ),
        "the ladder must park on its own without the breaker opening, got {:?}",
        wallet.circuit_breaker_state()
    );

    handle.close().await?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("snapshot wait failed: {0}")]
    SnapshotWait(#[from] SnapshotWaitError),
}
