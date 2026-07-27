//! `SyncSnapshot::last_observation` follows checked scan progress, not clean run endings.
//!
//! Every path to a `SyncOutcome` passes through the transparent-UTXO refresh, so a source
//! that never serves `transparent_utxos` makes clean completion unreachable: each attempt
//! keeps the blocks it committed and then faults in the post-commit tail. That is the shape a
//! Zinder deployment takes at the chain tip, where the epoch rotates once per block and the
//! tail outlives the pin. The wallet still knows exactly which chain state it committed and
//! when, and must report it.
//!
//! What it must not report is a chunk it never compared against the chain. The comparison is
//! the wallet's only detector for a corrupt commitment tree, and a spend built on one is
//! rejected by the network, so a chunk that outran the comparison leaves freshness where it
//! was.

use std::sync::Arc;

use zally_chain::{ChainSource, ChainSourceError};
use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zally_wallet::{
    RetryPolicy, SyncDriver, SyncDriverOptions, SyncDriverPhase, SyncRecoveryPolicy, WalletError,
};

use super::fixtures::{
    SnapshotWaitError, TestWalletError, TestWalletFixture, capture_sync_events, create_test_wallet,
    wait_for_snapshot,
};

fn fast_recovery_policy() -> SyncRecoveryPolicy {
    SyncRecoveryPolicy::default()
        .with_fault_backoff_initial_ms(20)
        .with_fault_backoff_cap_ms(40)
}

#[tokio::test]
async fn a_committed_chunk_publishes_its_observation_through_the_fault() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::none());

    let (_capture_guard, sync_events) = capture_sync_events();

    let tip = BlockHeight::from(50);
    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    chain_handle.serve_compact_blocks();
    chain_handle.advance_tip(tip);
    chain_handle.fail_transparent_utxos_next(1_024, || ChainSourceError::ChainEpochPinUnavailable);

    let driver = SyncDriver::new(
        wallet,
        chain as Arc<dyn ChainSource>,
        SyncDriverOptions::default()
            .with_poll_interval_ms(25)
            .with_recovery_policy(fast_recovery_policy()),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    let observed = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.last_observation.is_some()
    })
    .await?;
    let observation = observed.last_observation.ok_or(TestError::NoObservation)?;
    assert_eq!(observation.scanned_to_height, tip);
    assert!(observation.observed_at_ms > 0);
    assert!(
        sync_events.contains("wallet_sync_slow_progress"),
        "the observation must come from the chunk that committed and then faulted"
    );

    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn an_attempt_that_commits_nothing_publishes_no_observation() -> Result<(), TestError> {
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
        wallet,
        chain as Arc<dyn ChainSource>,
        SyncDriverOptions::default()
            .with_poll_interval_ms(25)
            .with_recovery_policy(fast_recovery_policy()),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    let parked = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, SyncDriverPhase::Parked { .. })
    })
    .await?;
    assert_eq!(
        parked.scanned_height, None,
        "the pin expired before any block reached storage"
    );
    assert_eq!(
        parked.last_observation, None,
        "an attempt that committed nothing observed nothing"
    );

    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn a_chunk_whose_roots_went_uncompared_publishes_no_observation() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::none());

    let tip = BlockHeight::from(50);
    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    chain_handle.serve_compact_blocks();
    chain_handle.advance_tip(tip);
    for _ in 0..10 {
        chain_handle.expire_epoch_on_tree_state_at(tip);
    }

    let driver = SyncDriver::new(
        wallet,
        chain as Arc<dyn ChainSource>,
        SyncDriverOptions::default()
            .with_poll_interval_ms(25)
            .with_recovery_policy(fast_recovery_policy()),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    let committed = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.scanned_height == Some(tip)
    })
    .await?;
    assert_eq!(
        committed.last_observation, None,
        "the chunk reached storage without a comparison against the chain"
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
    #[error("snapshot carried no observation")]
    NoObservation,
}
