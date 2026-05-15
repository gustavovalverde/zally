//! `Wallet::sync` advances scan progress to the chain tip (SYNC-1).

use zally_core::{BlockHeight, Network};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, MockChainSource, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn sync_catches_up_to_tip() -> Result<(), TestError> {
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
