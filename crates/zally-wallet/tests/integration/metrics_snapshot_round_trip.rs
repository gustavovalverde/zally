//! REQ-OBS-2 — `Wallet::metrics_snapshot` returns a typed snapshot of wallet state.

use zally_core::{BlockHeight, Network};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn metrics_snapshot_reports_network_and_account_count() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();
    let sealing = InMemorySealing::new();
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, _, _) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    let snapshot = wallet.metrics_snapshot().await?;
    assert_eq!(snapshot.network, network);
    assert_eq!(snapshot.account_count, 1);

    // Attaching an observer reflects in the snapshot.
    let _events = wallet.observe();
    let snapshot_after = wallet.metrics_snapshot().await?;
    assert!(snapshot_after.event_subscriber_count >= 1);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
