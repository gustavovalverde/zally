//! Calling `Wallet::builder(...).open()` with no prior `.create(...)` returns `NoSealedSeed`.

use zally_core::Network;
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions};
use zally_testkit::TempWalletPath;
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn open_without_seal_returns_no_sealed_seed() -> Result<(), std::io::Error> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    let outcome = Wallet::builder(network, sealing, storage).open().await;
    assert!(matches!(outcome, Err(WalletError::NoSealedSeed)));
    Ok(())
}
