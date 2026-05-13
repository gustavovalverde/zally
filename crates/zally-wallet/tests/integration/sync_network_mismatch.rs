//! `Wallet::sync` with a chain source on a different network fails closed.

use zally_core::{BlockHeight, Network};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, MockChainSource, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn sync_network_mismatch_fails_closed() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let regtest = Network::regtest_all_at_genesis();

    let sealing = InMemorySealing::new();
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let chain = zally_testkit::MockChainSource::new(regtest);
    let (wallet, _, _) =
        Wallet::create(&chain, regtest, sealing, storage, BlockHeight::from(1)).await?;

    let chain = MockChainSource::new(Network::Mainnet);
    let outcome = wallet.sync(&chain).await;
    assert!(matches!(
        outcome,
        Err(WalletError::NetworkMismatch {
            storage: _,
            requested: Network::Mainnet,
        })
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
