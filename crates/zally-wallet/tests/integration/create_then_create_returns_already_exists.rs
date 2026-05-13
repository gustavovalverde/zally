//! Calling `Wallet::create` twice against the same paths returns `AccountAlreadyExists`.

use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::TempWalletPath;
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn create_then_create_returns_already_exists() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let _ = Wallet::create(network, sealing, storage, BlockHeight::from(1)).await?;

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let outcome = Wallet::create(network, sealing, storage, BlockHeight::from(1)).await;
    assert!(matches!(outcome, Err(WalletError::AccountAlreadyExists)));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
