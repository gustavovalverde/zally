//! Network mismatch must fail closed at construction time, before any I/O.

use zally_core::{BlockHeight, Network};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn network_mismatch_fails_closed() -> Result<(), std::io::Error> {
    let temp = TempWalletPath::create()?;

    let sealing = InMemorySealing::new();
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        Network::Mainnet,
        temp.db_path(),
    ));

    let chain = zally_testkit::MockChainSource::new(Network::Testnet);
    let outcome = Wallet::create(
        &chain,
        Network::Testnet,
        sealing,
        storage,
        BlockHeight::from(1),
    )
    .await;
    assert!(matches!(
        outcome,
        Err(WalletError::NetworkMismatch {
            storage: Network::Mainnet,
            requested: Network::Testnet,
        })
    ));
    // No database was written because the network check ran before any I/O.
    assert!(!temp.db_path().exists());
    Ok(())
}
