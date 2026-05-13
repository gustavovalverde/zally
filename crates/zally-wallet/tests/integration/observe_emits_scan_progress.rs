//! `Wallet::observe()` receives `ScanProgress` events emitted during sync.

use zally_core::{BlockHeight, Network};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, MockChainSource, TempWalletPath};
use zally_wallet::{Wallet, WalletError, WalletEvent};

#[tokio::test]
async fn observe_emits_scan_progress() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = InMemorySealing::new();
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let (wallet, _, _) = Wallet::create(network, sealing, storage, BlockHeight::from(1)).await?;

    let mut events = wallet.observe();
    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(7));
    let _ = wallet.sync(&chain).await?;

    let first = events.next().await;
    assert!(matches!(first, Some(WalletEvent::ScanProgress { .. })));
    let second = events.next().await;
    assert!(matches!(
        second,
        Some(WalletEvent::ScanProgress {
            scanned_height,
            target_height,
        }) if scanned_height == BlockHeight::from(7) && target_height == BlockHeight::from(7)
    ));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
