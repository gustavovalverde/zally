//! `Wallet::capabilities()` reports the wallet's standing feature surface.

use zally_wallet::{Capability, SealingCapability, StorageCapability, WalletCapabilities};

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn capabilities_reports_standing_surface() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let caps: WalletCapabilities = wallet.capabilities();
    assert_eq!(caps.sealing, SealingCapability::AgeFile);
    assert_eq!(caps.storage, StorageCapability::Sqlite);
    assert_eq!(caps.network, network);
    assert!(
        caps.features
            .contains(&Capability::Zip311Draft1SaplingDisclosures)
    );
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
