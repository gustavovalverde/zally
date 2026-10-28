//! The sync driver records the chain-event cursor it has applied and resumes after it.
//!
//! A reorg the source announces while the wallet is stopped is replayed through the recorded
//! cursor and rewound at its fork point on the next start. A cursor the source no longer
//! retains, or never issued, is discarded and the driver replays the retained window, where a
//! reorg the wallet already followed rewinds nothing.

use std::sync::Arc;

use zally_chain::{ChainEventCursor, ChainEventStreamStart};
use zally_core::{BlockHeight, IdempotencyKey, TxId};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions, WalletStorage};
use zally_wallet::{SyncDriverPhase, SyncHandle, SyncSnapshot, Wallet, WalletEvent};

use super::reorg_fixtures::{
    ChainWallet, EventLog, FixtureError, chain_wallet, fast_driver_options, record_hold,
    start_driver, wait_for_driver,
};

async fn wait_until_scanned_to(
    handle: &SyncHandle,
    height: u32,
) -> Result<SyncSnapshot, FixtureError> {
    let mut seen = Vec::new();
    wait_for_driver(handle, &mut seen, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
            && snapshot.scanned_height == Some(BlockHeight::from(height))
    })
    .await
}

fn cursor_for(sequence: u64) -> Vec<u8> {
    sequence.to_be_bytes().to_vec()
}

fn rewinds(events: &EventLog) -> Vec<BlockHeight> {
    events
        .snapshot()
        .into_iter()
        .filter_map(|event| {
            if let WalletEvent::ReorgDetected {
                rolled_back_to_height,
                ..
            } = event
            {
                Some(rolled_back_to_height)
            } else {
                None
            }
        })
        .collect()
}

/// Closes the wallet and reopens it from its files, as a restarted process would.
async fn reopen(base: ChainWallet) -> Result<ChainWallet, FixtureError> {
    let ChainWallet {
        temp,
        wallet,
        storage,
        account_id,
        chain,
        chain_handle,
        network,
        seed,
    } = base;
    drop(wallet);
    drop(storage);
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let (wallet, _account_id) = Wallet::builder(network, sealing, storage.clone())
        .open()
        .await?;
    Ok(ChainWallet {
        temp,
        wallet,
        storage,
        account_id,
        chain,
        chain_handle,
        network,
        seed,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reorg_while_stopped_is_replayed_through_the_recorded_cursor() -> Result<(), FixtureError>
{
    let base = chain_wallet(30).await?;
    let withdrawal_key = IdempotencyKey::try_from("withdrawal-while-stopped".to_owned())
        .map_err(|_| FixtureError::Fixture("idempotency key"))?;
    base.storage
        .record_idempotent_submission(withdrawal_key.clone(), TxId::from_bytes([0xab; 32]))
        .await?;
    record_hold(&base.storage, base.account_id).await?;

    let handle = start_driver(&base.wallet, &base.chain, None, fast_driver_options())?;
    wait_until_scanned_to(&handle, 30).await?;
    handle.close().await?;
    let recorded_cursor = base.storage.find_chain_event_cursor().await?;
    assert_eq!(
        recorded_cursor,
        Some(cursor_for(base.chain_handle.latest_chain_event_sequence())),
        "the driver must record the cursor of the last event it applied"
    );
    let recorded_cursor = recorded_cursor.ok_or(FixtureError::Fixture("no recorded cursor"))?;

    base.chain_handle
        .trigger_reorg(BlockHeight::from(26), BlockHeight::from(32));
    base.chain_handle.advance_tip(BlockHeight::from(33));

    let base = reopen(base).await?;
    let events = EventLog::record(base.wallet.observe());
    let handle = start_driver(&base.wallet, &base.chain, None, fast_driver_options())?;
    wait_until_scanned_to(&handle, 33).await?;
    handle.close().await?;

    assert_eq!(
        base.chain_handle.chain_event_subscription_starts().last(),
        Some(&ChainEventStreamStart::AfterCursor(
            ChainEventCursor::from_bytes(recorded_cursor)
        )),
        "the restarted driver must resume after the recorded cursor"
    );
    assert_eq!(
        rewinds(&events),
        vec![BlockHeight::from(25)],
        "the replayed reorg must rewind to its fork point"
    );
    assert_eq!(
        base.storage
            .find_idempotent_submission(&withdrawal_key)
            .await?,
        Some(TxId::from_bytes([0xab; 32]))
    );
    assert_eq!(
        base.storage.list_active_holds(base.account_id).await?.len(),
        1
    );
    assert_eq!(
        base.storage.find_chain_event_cursor().await?,
        Some(cursor_for(base.chain_handle.latest_chain_event_sequence()))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_reorg_the_wallet_already_followed_rewinds_nothing() -> Result<(), FixtureError>
{
    let base = chain_wallet(20).await?;
    let first_run = EventLog::record(base.wallet.observe());
    let handle = start_driver(&base.wallet, &base.chain, None, fast_driver_options())?;
    wait_until_scanned_to(&handle, 20).await?;
    base.chain_handle
        .trigger_reorg(BlockHeight::from(15), BlockHeight::from(22));
    wait_until_scanned_to(&handle, 22).await?;
    handle.close().await?;
    assert_eq!(rewinds(&first_run), vec![BlockHeight::from(14)]);

    base.storage.clear_chain_event_cursor().await?;
    let second_run = EventLog::record(base.wallet.observe());
    let handle = start_driver(&base.wallet, &base.chain, None, fast_driver_options())?;
    wait_until_scanned_to(&handle, 22).await?;
    handle.close().await?;
    assert_eq!(
        base.chain_handle.chain_event_subscription_starts().last(),
        Some(&ChainEventStreamStart::EarliestRetained)
    );
    assert!(
        rewinds(&second_run).is_empty(),
        "a replayed reorg the wallet already followed must not rewind it"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_cursor_replays_the_retained_window() -> Result<(), FixtureError> {
    let base = chain_wallet(10).await?;
    for tip in 11..=15 {
        base.chain_handle.advance_tip(BlockHeight::from(tip));
    }
    base.storage
        .record_chain_event_cursor(cursor_for(2))
        .await?;
    base.chain_handle.expire_chain_events_through(4);

    let handle = start_driver(&base.wallet, &base.chain, None, fast_driver_options())?;
    wait_until_scanned_to(&handle, 15).await?;
    let latest = base.chain_handle.latest_chain_event_sequence();
    wait_for_recorded_cursor(&base.storage, cursor_for(latest)).await?;
    handle.close().await?;

    let starts = base.chain_handle.chain_event_subscription_starts();
    assert_eq!(
        starts.get(..2),
        Some(
            &[
                ChainEventStreamStart::AfterCursor(ChainEventCursor::from_bytes(cursor_for(2))),
                ChainEventStreamStart::EarliestRetained,
            ][..]
        ),
        "an expired cursor must be replaced by the retained window"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unrecognized_cursor_is_discarded() -> Result<(), FixtureError> {
    let base = chain_wallet(10).await?;
    base.chain_handle.advance_tip(BlockHeight::from(12));
    base.storage
        .record_chain_event_cursor(b"issued by another store".to_vec())
        .await?;

    let handle = start_driver(&base.wallet, &base.chain, None, fast_driver_options())?;
    wait_until_scanned_to(&handle, 12).await?;
    let latest = base.chain_handle.latest_chain_event_sequence();
    wait_for_recorded_cursor(&base.storage, cursor_for(latest)).await?;
    handle.close().await?;

    let starts = base.chain_handle.chain_event_subscription_starts();
    assert_eq!(
        starts.get(..2),
        Some(
            &[
                ChainEventStreamStart::AfterCursor(ChainEventCursor::from_bytes(
                    b"issued by another store".to_vec()
                )),
                ChainEventStreamStart::EarliestRetained,
            ][..]
        ),
        "an unrecognized cursor must be replaced by the retained window"
    );
    Ok(())
}

async fn wait_for_recorded_cursor(storage: &Sqlite, expected: Vec<u8>) -> Result<(), FixtureError> {
    let storage = Arc::new(storage.clone());
    tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        loop {
            if storage.find_chain_event_cursor().await.ok().flatten() == Some(expected.clone()) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_elapsed| FixtureError::Fixture("timed out waiting for the recorded cursor"))
}
