//! `Wallet::capabilities()` reports the wallet's standing feature surface.

use zally_core::{BlockHeight, Network};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, TempWalletPath};
use zally_wallet::{
    Capability, SealingCapability, StorageCapability, Wallet, WalletCapabilities, WalletError,
};

#[tokio::test]
async fn capabilities_reports_standing_surface() -> Result<(), TestError> {
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

    let caps: WalletCapabilities = wallet.capabilities();
    assert_eq!(caps.sealing, SealingCapability::InMemory);
    assert_eq!(caps.storage, StorageCapability::Sqlite);
    assert_eq!(caps.network, network);
    assert!(caps.features.contains(&Capability::Zip316UnifiedAddresses));
    assert!(caps.features.contains(&Capability::Zip302Memos));
    assert!(caps.features.contains(&Capability::Zip320TexAddresses));
    assert!(caps.features.contains(&Capability::Zip317ConventionalFee));
    assert!(caps.features.contains(&Capability::SyncIncremental));
    assert!(caps.features.contains(&Capability::SyncDriver));
    assert!(caps.features.contains(&Capability::EventStream));
    assert!(caps.features.contains(&Capability::IdempotentSend));
    assert!(caps.features.contains(&Capability::PcztV06));
    assert!(caps.features.contains(&Capability::MetricsSnapshot));
    assert!(caps.features.contains(&Capability::StatusSnapshot));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
