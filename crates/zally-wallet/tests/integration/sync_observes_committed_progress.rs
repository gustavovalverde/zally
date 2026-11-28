//! `SyncSnapshot::last_observation` follows checked scan progress, not clean run endings.
//!
//! An attempt's blocks are committed by `WalletStorage::scan_blocks` before the wallet
//! compares its commitment-tree roots against the chain, so a rotated epoch pin or any other
//! fault can land on either side of that comparison. The wallet still knows exactly which
//! chain state it committed and when, and must report it once the comparison has actually
//! run.
//!
//! What it must not report is a chunk it never compared against the chain. The comparison is
//! the wallet's only detector for a corrupt commitment tree, and a spend built on one is
//! rejected by the network, so a chunk that outran the comparison leaves freshness where it
//! was.

use std::sync::Arc;

use incrementalmerkletree::{Hashable as _, frontier::CommitmentTree};
use zally_chain::ChainSource;
use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zally_wallet::{
    RetryPolicy, SyncDriver, SyncDriverOptions, SyncDriverPhase, SyncRecoveryPolicy, WalletError,
};

use super::fixtures::{
    SnapshotWaitError, TestWalletError, TestWalletFixture, create_test_wallet, wait_for_snapshot,
};

fn fast_recovery_policy() -> SyncRecoveryPolicy {
    SyncRecoveryPolicy::default()
        .with_fault_backoff_initial_ms(20)
        .with_fault_backoff_cap_ms(40)
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
    for _ in 0..15 {
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

    let faulted = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot
            .last_fault
            .as_ref()
            .is_some_and(|fault| fault.consecutive_faults >= 10)
    })
    .await?;
    assert_eq!(
        faulted.scanned_height, None,
        "the pin expired before any block reached storage"
    );
    assert_eq!(
        faulted.last_observation, None,
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

#[tokio::test]
async fn a_restarted_driver_verifies_a_wallet_already_at_tip() -> Result<(), TestError> {
    let (fixture, chain, tip) = wallet_scanned_to_tip().await?;
    let driver = SyncDriver::new(
        fixture.wallet,
        chain,
        SyncDriverOptions::default().with_poll_interval_ms(60_000),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();
    let waiting = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
    })
    .await?;
    assert_eq!(
        waiting
            .last_observation
            .map(|observation| observation.scanned_to_height),
        Some(tip),
    );
    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn a_restarted_driver_detects_a_replaced_tip_even_with_empty_trees() -> Result<(), TestError>
{
    let (fixture, chain, tip) = wallet_scanned_to_tip().await?;
    chain.handle().reorg_silently(tip, tip);
    let driver = SyncDriver::new(
        fixture.wallet,
        chain,
        SyncDriverOptions::default()
            .with_poll_interval_ms(60_000)
            .with_recovery_policy(
                SyncRecoveryPolicy::default().with_fault_backoff_initial_ms(5_000),
            ),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();
    let faulted = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.last_fault.is_some() || snapshot.phase == SyncDriverPhase::Waiting
    })
    .await?;
    assert!(
        faulted
            .last_fault
            .as_ref()
            .is_some_and(|fault| fault.repair == zally_wallet::SyncRepair::Rewind)
    );
    assert_eq!(faulted.last_observation, None);
    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn a_restarted_driver_withholds_an_observation_without_a_pool_root() -> Result<(), TestError>
{
    let (fixture, chain, tip) = wallet_scanned_to_tip().await?;
    let epoch = chain.current_epoch().await.map_err(WalletError::from)?;
    let mut tree_state = chain
        .tree_state_at(epoch, tip)
        .await
        .map_err(WalletError::from)?;
    let mut sapling =
        CommitmentTree::<sapling::Node, { sapling::NOTE_COMMITMENT_TREE_DEPTH }>::empty();
    assert!(sapling.append(sapling::Node::empty_leaf()).is_ok());
    tree_state.sapling_final_state_bytes.clear();
    zcash_primitives::merkle_tree::write_commitment_tree(
        &sapling,
        &mut tree_state.sapling_final_state_bytes,
    )?;
    chain.handle().serve_tree_state(tree_state);

    let driver = SyncDriver::new(
        fixture.wallet,
        chain,
        SyncDriverOptions::default().with_poll_interval_ms(60_000),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();
    let waiting = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
    })
    .await?;
    assert_eq!(waiting.scanned_height, Some(tip));
    assert_eq!(waiting.last_observation, None);
    handle.close().await?;
    Ok(())
}

async fn wallet_scanned_to_tip()
-> Result<(TestWalletFixture, Arc<MockChainSource>, BlockHeight), TestError> {
    let fixture = create_test_wallet().await?;
    let chain = Arc::new(MockChainSource::new(fixture.wallet.network()));
    let chain_handle = chain.handle();
    chain_handle.serve_compact_blocks();
    let tip = BlockHeight::from(50);
    chain_handle.advance_tip(tip);
    assert_eq!(
        fixture.wallet.sync(chain.as_ref()).await?.scanned_to_height,
        tip
    );
    assert_eq!(fixture.wallet.sync(chain.as_ref()).await?.block_count, 0);
    Ok((fixture, chain, tip))
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("frontier serialization failed: {0}")]
    Frontier(#[from] std::io::Error),
    #[error("test wallet error: {0}")]
    Fixture(#[from] TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("snapshot wait failed: {0}")]
    SnapshotWait(#[from] SnapshotWaitError),
}
