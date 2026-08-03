//! Regression: a source that rotates its chain epoch under a running attempt must not stall
//! the wallet, on either the block-scan path or the transparent-UTXO refresh path.
//!
//! Zinder retains exactly one epoch and rotates it on every canonical append, so a wallet far
//! enough behind meets [`ChainSourceError::ChainEpochPinUnavailable`] on every attempt whose
//! scan outlives one block. The pin expiry is a restart signal, not backend trouble: the
//! breaker must stay closed, the ladder must not park on it alone, and every attempt must
//! keep the blocks it committed before the pin expired.
//!
//! [`Wallet::refresh_transparent_utxos`] pins its own epoch, independent of the block-scan
//! path; the tests here also cover its half of the same contract: a rotation surfaces as
//! itself rather than a silently mixed result, and every receiver visited by one attempt
//! observes the same pinned epoch.

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
async fn refresh_transparent_utxos_surfaces_a_rotated_pin_as_itself() -> Result<(), TestWalletError>
{
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::none());

    let chain = MockChainSource::new(network);
    let chain_handle = chain.handle();
    chain_handle.advance_tip(BlockHeight::from(50));
    chain_handle.fail_transparent_utxos_next(10, || ChainSourceError::ChainEpochPinUnavailable);

    for _ in 0..10 {
        let outcome = wallet.refresh_transparent_utxos(&chain).await;
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

/// Every receiver read inside one `refresh_transparent_utxos` attempt must observe the same
/// pinned epoch.
///
/// Holds even when the wallet has more than one transparent receiver: nothing in the walk
/// re-pins mid-attempt, so a result mixing two epochs cannot arise.
#[tokio::test]
async fn refresh_transparent_utxos_pins_one_epoch_across_every_receiver() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    // A fresh account already carries several dozen pre-generated transparent receivers
    // (external, internal, and ephemeral scopes), more than enough to prove the invariant
    // without deriving any more.
    let chain = MockChainSource::new(network);
    let chain_handle = chain.handle();
    chain_handle.advance_tip(BlockHeight::from(50));

    let epoch_ids_before = chain_handle.artifact_epoch_ids().len();
    let outcome = wallet.refresh_transparent_utxos(&chain).await?;
    assert!(
        outcome.receivers_visited > 1,
        "the fixture wallet must carry more than one transparent receiver for this proof to \
         mean anything, visited {}",
        outcome.receivers_visited
    );

    let recorded = chain_handle.artifact_epoch_ids();
    let this_attempt = &recorded[epoch_ids_before..];
    assert_eq!(
        u64::try_from(this_attempt.len()).unwrap_or(u64::MAX),
        outcome.receivers_visited,
        "one transparent_utxos read per receiver"
    );
    assert!(
        this_attempt
            .iter()
            .all(|epoch_id| *epoch_id == this_attempt[0]),
        "every receiver in one walk must observe the same pinned epoch, got {this_attempt:?}"
    );
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
    // Every chunk's tree-root check meets a rotated pin right after its blocks commit, the
    // shape a Zinder deployment takes at the chain tip.
    for chunk in 1..=chunk_count {
        chain_handle
            .expire_epoch_on_tree_state_at(BlockHeight::from(BLOCKS_PER_SYNC_CHUNK * chunk));
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
    // Comfortably above the restartable-fault threshold the ladder tolerates before
    // parking (`SyncRecoveryPolicy::restartable_escalate_after_faults`, default 10; parking
    // needs one fault past it).
    for _ in 0..15 {
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
