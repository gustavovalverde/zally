//! Regression: `Wallet::get_pending_transparent_inputs` reflects rows recorded directly via
//! storage, honours the configured inflight window, and clears on confirmation.

use zally_chain::ChainSource as _;
use zally_core::{BlockHeight, OutPoint, TxId, Zatoshis};
use zally_storage::{PendingBroadcastRecord, Sqlite, SqliteOptions, WalletStorage};
use zally_testkit::MockChainSource;
use zally_wallet::{WalletError, WalletOptions};

use super::fixtures::{TestWalletFixture, create_test_wallet, create_test_wallet_with_options};

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_u64, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[tokio::test]
async fn get_pending_transparent_inputs_returns_empty_on_fresh_wallet() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let snapshot = wallet.get_pending_transparent_inputs(account_id).await?;
    assert!(
        snapshot.inputs.is_empty(),
        "fresh wallet has no pending inputs"
    );
    assert_eq!(snapshot.network, network);
    assert_eq!(snapshot.account_id, account_id);
    assert!(snapshot.as_of_height.is_none());
    Ok(())
}

#[tokio::test]
async fn get_pending_transparent_inputs_reports_recorded_rows() -> Result<(), TestError> {
    let TestWalletFixture {
        temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    storage.open_or_create().await?;

    let now_ms = unix_ms_now();
    let broadcast_tx_id = TxId::from_bytes([1_u8; 32]);
    let outpoint = OutPoint::new(TxId::from_bytes([2_u8; 32]), 7);
    storage
        .record_pending_broadcast_inputs(PendingBroadcastRecord {
            broadcast_tx_id,
            account_id,
            broadcast_at_ms: now_ms,
            broadcast_at_height: Some(BlockHeight::from(42)),
            inputs: vec![(
                outpoint,
                Zatoshis::try_from(12_345_u64).unwrap_or(Zatoshis::zero()),
            )],
        })
        .await?;

    let snapshot = wallet.get_pending_transparent_inputs(account_id).await?;
    assert_eq!(snapshot.inputs.len(), 1);
    let entry = &snapshot.inputs[0];
    assert_eq!(entry.outpoint, outpoint);
    assert_eq!(entry.broadcast_tx_id, broadcast_tx_id);
    assert_eq!(entry.broadcast_at_ms, now_ms);
    assert_eq!(entry.broadcast_at_height, Some(BlockHeight::from(42)));
    assert_eq!(entry.value_zat.as_u64(), 12_345);
    Ok(())
}

#[tokio::test]
async fn get_pending_transparent_inputs_drops_rows_outside_window() -> Result<(), TestError> {
    let options = WalletOptions::default().with_pending_broadcast_window_ms(60_000);
    let TestWalletFixture {
        temp,
        wallet,
        account_id,
    } = create_test_wallet_with_options(options).await?;
    let network = wallet.network();
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    storage.open_or_create().await?;

    let stale_at_ms = unix_ms_now().saturating_sub(10 * 60_000);
    storage
        .record_pending_broadcast_inputs(PendingBroadcastRecord {
            broadcast_tx_id: TxId::from_bytes([3_u8; 32]),
            account_id,
            broadcast_at_ms: stale_at_ms,
            broadcast_at_height: None,
            inputs: vec![(
                OutPoint::new(TxId::from_bytes([4_u8; 32]), 0),
                Zatoshis::try_from(100_u64).unwrap_or(Zatoshis::zero()),
            )],
        })
        .await?;

    let snapshot = wallet.get_pending_transparent_inputs(account_id).await?;
    assert!(
        snapshot.inputs.is_empty(),
        "stale rows must fall outside the window"
    );
    Ok(())
}

#[tokio::test]
async fn sync_clears_expired_pending_broadcast_rows() -> Result<(), TestError> {
    let options = WalletOptions::default().with_pending_broadcast_window_ms(60_000);
    let TestWalletFixture {
        temp,
        wallet,
        account_id,
    } = create_test_wallet_with_options(options).await?;
    let network = wallet.network();
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    storage.open_or_create().await?;

    let stale_at_ms = unix_ms_now().saturating_sub(10 * 60_000);
    let tx_id = TxId::from_bytes([5_u8; 32]);
    storage
        .record_pending_broadcast_inputs(PendingBroadcastRecord {
            broadcast_tx_id: tx_id,
            account_id,
            broadcast_at_ms: stale_at_ms,
            broadcast_at_height: None,
            inputs: vec![(
                OutPoint::new(TxId::from_bytes([6_u8; 32]), 0),
                Zatoshis::try_from(100_u64).unwrap_or(Zatoshis::zero()),
            )],
        })
        .await?;

    let direct_rows = storage.list_pending_broadcast_inputs(account_id, 0).await?;
    assert_eq!(direct_rows.len(), 1, "row must persist before sync");

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(10));
    wallet.sync(&chain).await?;
    let _ = chain.safe_chain_tip().await;

    let after_sync = storage.list_pending_broadcast_inputs(account_id, 0).await?;
    assert!(
        after_sync.is_empty(),
        "sync must drop expired rows via clear_expired_pending_broadcast_inputs"
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
    Storage(#[from] zally_storage::StorageError),
}
