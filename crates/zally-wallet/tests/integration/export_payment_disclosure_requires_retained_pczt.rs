//! Payment-disclosure export requires retained finalized PCZT source material.

use zally_core::{Network, PaymentRecipient, TxId, Zatoshis};
use zally_pczt::PcztBytes;
use zally_wallet::{ExportPaymentDisclosurePlan, WalletError};
use zcash_keys::{
    address::Address,
    keys::{UnifiedAddressRequest, UnifiedSpendingKey},
};
use zcash_payment_disclosure::PaymentDisclosureProfile;

use super::fixtures::{TestWalletError, create_test_wallet};

#[tokio::test]
async fn export_payment_disclosure_requires_retained_pczt() -> Result<(), TestError> {
    let fixture = create_test_wallet().await?;
    let transaction_id = TxId::from_bytes([0x71; 32]);
    let network = Network::regtest();
    let params = network.to_parameters();
    let unified_address =
        UnifiedSpendingKey::from_seed(&params, &[0x51; 32], zip32::AccountId::ZERO)
            .map_err(|err| TestError::setup(format!("spending-key derivation failed: {err}")))?
            .to_unified_full_viewing_key()
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|err| TestError::setup(format!("unified-address derivation failed: {err}")))?
            .0;
    let sapling_recipient = unified_address
        .sapling()
        .copied()
        .ok_or_else(|| TestError::setup("fixture Unified Address has no Sapling receiver"))?;
    let plan = ExportPaymentDisclosurePlan::new(
        transaction_id,
        PaymentRecipient::SaplingAddress {
            encoded: Address::Sapling(sapling_recipient).encode(&params),
            network,
        },
        Zatoshis::try_from(1_u64)?,
        b"merchant-challenge".to_vec(),
        PaymentDisclosureProfile::Zip311Draft1,
    );

    let outcome = fixture.wallet.export_payment_disclosure(plan).await;
    assert!(matches!(
        outcome,
        Err(WalletError::PaymentDisclosureSourceMissing {
            transaction_id: missing_transaction_id,
        }) if missing_transaction_id == transaction_id
    ));
    Ok(())
}

#[tokio::test]
async fn export_payment_disclosure_refuses_non_sapling_recipient_before_source_lookup()
-> Result<(), TestError> {
    let fixture = create_test_wallet().await?;
    let plan = ExportPaymentDisclosurePlan::new(
        TxId::from_bytes([0x72; 32]),
        PaymentRecipient::UnifiedAddress {
            encoded: "uregtest1recipient".to_owned(),
            network: Network::regtest(),
        },
        Zatoshis::try_from(1_u64)?,
        b"merchant-challenge".to_vec(),
        PaymentDisclosureProfile::Zip311Draft1,
    );

    let outcome = fixture.wallet.export_payment_disclosure(plan).await;
    assert!(matches!(
        outcome,
        Err(WalletError::PaymentDisclosureExport(
            zally_pczt::PaymentDisclosureExportError::RecipientUnsupported
        ))
    ));
    Ok(())
}

#[tokio::test]
async fn export_payment_disclosure_accepts_a_unified_address_with_a_sapling_receiver()
-> Result<(), TestError> {
    let fixture = create_test_wallet().await?;
    let network = Network::regtest();
    let params = network.to_parameters();
    let unified_address =
        UnifiedSpendingKey::from_seed(&params, &[0x54; 32], zip32::AccountId::ZERO)
            .map_err(|err| TestError::setup(format!("spending-key derivation failed: {err}")))?
            .to_unified_full_viewing_key()
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|err| TestError::setup(format!("unified-address derivation failed: {err}")))?
            .0;
    assert!(unified_address.sapling().is_some());
    let transaction_id = TxId::from_bytes([0x73; 32]);
    let plan = ExportPaymentDisclosurePlan::new(
        transaction_id,
        PaymentRecipient::UnifiedAddress {
            encoded: Address::Unified(unified_address).encode(&params),
            network,
        },
        Zatoshis::try_from(1_u64)?,
        b"merchant-challenge".to_vec(),
        PaymentDisclosureProfile::Zip311Draft1,
    );

    let outcome = fixture.wallet.export_payment_disclosure(plan).await;
    assert!(matches!(
        outcome,
        Err(WalletError::PaymentDisclosureSourceMissing {
            transaction_id: missing_transaction_id,
        }) if missing_transaction_id == transaction_id
    ));
    Ok(())
}

#[tokio::test]
async fn extract_pczt_rejects_network_mismatch_before_storage() -> Result<(), TestError> {
    let fixture = create_test_wallet().await?;
    let foreign_pczt = PcztBytes::from_serialized(vec![0_u8; 4], Network::Mainnet);

    let outcome = fixture.wallet.extract_pczt(foreign_pczt).await;
    assert!(matches!(
        outcome,
        Err(WalletError::NetworkMismatch {
            storage: Network::Regtest { .. },
            requested: Network::Mainnet,
        })
    ));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("wallet fixture error: {0}")]
    Fixture(#[from] TestWalletError),
    #[error("zatoshis error: {0}")]
    Zatoshis(#[from] zally_core::ZatoshisError),
    #[error("fixture error: {0}")]
    Setup(String),
}

impl TestError {
    fn setup(message: impl Into<String>) -> Self {
        Self::Setup(message.into())
    }
}
