//! Calling `Wallet::open` with no prior `Wallet::create` returns `NoSealedSeed`.

use zally_core::Network;
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::TempWalletPath;
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn open_without_seal_returns_no_sealed_seed() -> Result<(), std::io::Error> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let outcome = Wallet::open(network, sealing, storage).await;
    assert!(matches!(outcome, Err(WalletError::NoSealedSeed)));
    Ok(())
}
