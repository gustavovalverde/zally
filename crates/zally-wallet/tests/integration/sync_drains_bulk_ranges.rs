//! A wallet far below the stable region drains bulk ranges within one wakeup.
//!
//! Once the wallet holds commitment-tree shard metadata and sits more than `PRUNING_DEPTH`
//! blocks below the chain tip, recording the tip queues a ten-block `Verify` range that
//! outranks every bulk range. Recording it again between chunks re-queues that lookahead, so
//! a wakeup that re-records per chunk never advances more than ten blocks at a time. One
//! record per wakeup leaves the bulk `ChainTip` range at the head of the queue for every
//! chunk after the first.
//!
//! Shard metadata is planted through the storage call the subtree-root backfill uses: the
//! mock chain serves transactionless blocks, so no scan of it ever completes a subtree.
//!
//! The other half of that split is what a drained queue does not mean: it is drained against
//! the tip the opening record captured, which the chain may have passed while the drain ran.

use zally_core::BlockHeight;
use zally_storage::{Sqlite, SqliteOptions, StorageError, WalletStorage as _};
use zally_testkit::MockChainSource;
use zally_wallet::WalletError;
use zcash_protocol::ShieldedPool;

use super::fixtures::{TestWalletFixture, create_test_wallet};

/// `zcash_client_sqlite::VERIFY_LOOKAHEAD`: the width of the `Verify` range that recording a
/// chain tip queues above the scanned frontier.
const VERIFY_LOOKAHEAD: u64 = 10;

/// Height the wallet scans to before the chain runs far ahead of it.
const SETTLED_HEIGHT: u32 = 200;

/// Chain tip after the wallet falls behind: far enough above `SETTLED_HEIGHT` that the
/// frontier sits below `tip - PRUNING_DEPTH`.
const DISTANT_TIP: u32 = 5_000;

#[tokio::test]
async fn one_wakeup_drains_a_range_wider_than_the_verify_lookahead() -> Result<(), TestError> {
    let TestWalletFixture {
        temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    let chain = MockChainSource::new(network);

    chain
        .handle()
        .advance_tip(BlockHeight::from(SETTLED_HEIGHT));
    while wallet.sync(&chain).await?.block_count > 0 {}

    let shards = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    shards.open_or_create().await?;
    shards
        .put_subtree_roots(
            ShieldedPool::Sapling,
            0,
            vec![(BlockHeight::from(SETTLED_HEIGHT / 2), [0_u8; 32])],
        )
        .await?;
    drop(shards);

    chain.handle().advance_tip(BlockHeight::from(DISTANT_TIP));

    let verify = wallet.sync(&chain).await?;
    assert_eq!(
        verify.block_count, VERIFY_LOOKAHEAD,
        "recording the tip must queue the tip-adjacent verify lookahead first"
    );

    let bulk = wallet.scan_queued_range(&chain).await?;
    assert!(
        bulk.block_count > VERIFY_LOOKAHEAD,
        "the chunk after the verify lookahead must drain the bulk range, scanned {} blocks",
        bulk.block_count
    );
    Ok(())
}

/// Height the chain reaches while the wallet is busy draining the queue below it.
const TIP_REACHED_DURING_DRAIN: u32 = 260;

#[tokio::test]
async fn a_drained_queue_leaves_the_recorded_tip_where_the_drain_started() -> Result<(), TestError>
{
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let chain = MockChainSource::new(wallet.network());

    chain
        .handle()
        .advance_tip(BlockHeight::from(SETTLED_HEIGHT));
    while wallet.sync(&chain).await?.block_count > 0 {}

    chain
        .handle()
        .advance_tip(BlockHeight::from(TIP_REACHED_DURING_DRAIN));

    let drained = wallet.scan_queued_range(&chain).await?;
    assert_eq!(
        drained.block_count, 0,
        "the queue holds nothing below the tip the opening record captured"
    );
    assert_eq!(
        wallet.status_snapshot().await?.visible_tip_height,
        Some(BlockHeight::from(SETTLED_HEIGHT)),
        "a drained queue must not be read as caught up: the wallet still holds the tip the \
         drain started against, and a spend built now would carry an expiry the chain has passed"
    );

    let recorded = wallet.sync(&chain).await?;
    assert!(
        recorded.block_count > 0,
        "recording the tip again must queue the blocks mined during the drain"
    );
    assert_eq!(
        wallet.status_snapshot().await?.visible_tip_height,
        Some(BlockHeight::from(TIP_REACHED_DURING_DRAIN))
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] super::fixtures::TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}
