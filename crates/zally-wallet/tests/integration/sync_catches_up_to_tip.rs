//! REQ-SYNC-1 — `Wallet::sync` advances scan progress to the chain tip.

use zally_core::{BlockHeight, Network};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, MockChainSource, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn sync_catches_up_to_tip() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = InMemorySealing::new();
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let (wallet, _, _) = Wallet::create(network, sealing, storage, BlockHeight::from(1)).await?;

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(42));

    let outcome = wallet.sync(&chain).await?;
    assert_eq!(outcome.scanned_to_height, BlockHeight::from(42));
    assert_eq!(outcome.reorgs_observed, 0);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
