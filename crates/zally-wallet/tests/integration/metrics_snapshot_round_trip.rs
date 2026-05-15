//! `Wallet::metrics_snapshot` returns a typed snapshot of wallet state (OBS-2).

use zally_core::{BlockHeight, Network};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, TempWalletPath};
use zally_wallet::{SyncStatus, Wallet, WalletError};

#[tokio::test]
async fn metrics_snapshot_reports_network_and_account_count() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();
    let sealing = InMemorySealing::new();
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, _, _) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    let snapshot = wallet.metrics_snapshot().await?;
    assert_eq!(snapshot.network, network);
    assert_eq!(snapshot.account_count, 1);
    assert_eq!(snapshot.scanned_height, None);
    assert_eq!(snapshot.chain_tip_height, None);
    assert_eq!(snapshot.lag_blocks, None);

    // Attaching an observer reflects in the snapshot.
    let _events = wallet.observe();
    let snapshot_after = wallet.metrics_snapshot().await?;
    assert!(snapshot_after.event_subscriber_count >= 1);
    Ok(())
}

#[tokio::test]
async fn status_snapshot_reports_observed_tip_after_sync() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();
    let sealing = InMemorySealing::new();
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, _, _) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    chain.handle().advance_tip(BlockHeight::from(42));
    wallet.sync(&chain).await?;

    let status = wallet.status_snapshot().await?;
    assert_eq!(status.network, network);
    assert_eq!(status.scanned_height, None);
    assert_eq!(status.chain_tip_height, Some(BlockHeight::from(42)));
    assert_eq!(
        status.sync_status,
        SyncStatus::Starting {
            target_height: BlockHeight::from(42)
        }
    );

    let metrics = wallet.metrics_snapshot().await?;
    assert_eq!(metrics.chain_tip_height, Some(BlockHeight::from(42)));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
