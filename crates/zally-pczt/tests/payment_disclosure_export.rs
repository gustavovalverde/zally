//! Public payment-disclosure export behavior at the PCZT boundary.

use zally_core::{Network, PaymentRecipient, TxId, Zatoshis};
use zally_keys::{Mnemonic, SeedMaterial};
use zally_pczt::{
    PaymentDisclosureExportError, PaymentDisclosureExportPlan, PcztBytes, export_payment_disclosure,
};
use zcash_keys::{
    address::Address,
    keys::{UnifiedAddressRequest, UnifiedSpendingKey},
};
use zcash_payment_disclosure::PaymentDisclosureProfile;

#[test]
fn export_refuses_a_pczt_without_sapling_spend_authority() -> Result<(), TestError> {
    let network = Network::regtest();
    let pczt =
        pczt::roles::creator::Creator::new(0xC2D6_D0B4, 100, 1, Some([0; 32]), Some([0; 32]))
            .map_err(|err| TestError::fixture(format!("creator init failed: {err:?}")))?
            .build()
            .map_err(|err| TestError::fixture(format!("creator build failed: {err:?}")))?;
    let finalized_pczt = PcztBytes::from_pczt(pczt, network)?;
    let params = network.to_parameters();
    let unified_address =
        UnifiedSpendingKey::from_seed(&params, &[0x52; 32], zip32::AccountId::ZERO)
            .map_err(|err| TestError::fixture(format!("spending-key derivation failed: {err}")))?
            .to_unified_full_viewing_key()
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|err| TestError::fixture(format!("unified-address derivation failed: {err}")))?
            .0;
    let sapling_recipient = unified_address
        .sapling()
        .copied()
        .ok_or_else(|| TestError::fixture("fixture Unified Address has no Sapling receiver"))?;
    let plan = PaymentDisclosureExportPlan::new(
        TxId::from_bytes([0x44; 32]),
        PaymentRecipient::SaplingAddress {
            encoded: Address::Sapling(sapling_recipient).encode(&params),
            network,
        },
        Zatoshis::try_from(1_u64)?,
        b"merchant-challenge".to_vec(),
        PaymentDisclosureProfile::Zip311Draft1,
    );
    let seed = SeedMaterial::from_mnemonic(&Mnemonic::generate(), "");

    let outcome = export_payment_disclosure(&finalized_pczt, plan, &seed);
    assert!(matches!(
        outcome,
        Err(PaymentDisclosureExportError::SaplingSpendsMissing)
    ));
    Ok(())
}

#[test]
fn export_refuses_a_non_sapling_recipient() -> Result<(), TestError> {
    let network = Network::regtest();
    let pczt =
        pczt::roles::creator::Creator::new(0xC2D6_D0B4, 100, 1, Some([0; 32]), Some([0; 32]))
            .map_err(|err| TestError::fixture(format!("creator init failed: {err:?}")))?
            .build()
            .map_err(|err| TestError::fixture(format!("creator build failed: {err:?}")))?;
    let finalized_pczt = PcztBytes::from_pczt(pczt, network)?;
    let plan = PaymentDisclosureExportPlan::new(
        TxId::from_bytes([0x45; 32]),
        PaymentRecipient::UnifiedAddress {
            encoded: "uregtest1recipient".to_owned(),
            network,
        },
        Zatoshis::try_from(1_u64)?,
        b"merchant-challenge".to_vec(),
        PaymentDisclosureProfile::Zip311Draft1,
    );
    let seed = SeedMaterial::from_mnemonic(&Mnemonic::generate(), "");

    let outcome = export_payment_disclosure(&finalized_pczt, plan, &seed);
    assert!(matches!(
        outcome,
        Err(PaymentDisclosureExportError::RecipientUnsupported)
    ));
    Ok(())
}

#[test]
fn export_accepts_a_unified_address_with_a_sapling_receiver() -> Result<(), TestError> {
    let network = Network::regtest();
    let pczt =
        pczt::roles::creator::Creator::new(0xC2D6_D0B4, 100, 1, Some([0; 32]), Some([0; 32]))
            .map_err(|err| TestError::fixture(format!("creator init failed: {err:?}")))?
            .build()
            .map_err(|err| TestError::fixture(format!("creator build failed: {err:?}")))?;
    let finalized_pczt = PcztBytes::from_pczt(pczt, network)?;
    let params = network.to_parameters();
    let unified_address =
        UnifiedSpendingKey::from_seed(&params, &[0x53; 32], zip32::AccountId::ZERO)
            .map_err(|err| TestError::fixture(format!("spending-key derivation failed: {err}")))?
            .to_unified_full_viewing_key()
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|err| TestError::fixture(format!("unified-address derivation failed: {err}")))?
            .0;
    assert!(unified_address.sapling().is_some());
    let plan = PaymentDisclosureExportPlan::new(
        TxId::from_bytes([0x46; 32]),
        PaymentRecipient::UnifiedAddress {
            encoded: Address::Unified(unified_address).encode(&params),
            network,
        },
        Zatoshis::try_from(1_u64)?,
        b"merchant-challenge".to_vec(),
        PaymentDisclosureProfile::Zip311Draft1,
    );
    let seed = SeedMaterial::from_mnemonic(&Mnemonic::generate(), "");

    let outcome = export_payment_disclosure(&finalized_pczt, plan, &seed);
    assert!(matches!(
        outcome,
        Err(PaymentDisclosureExportError::SaplingSpendsMissing)
    ));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("PCZT error: {0}")]
    Pczt(#[from] zally_pczt::PcztError),
    #[error("zatoshis error: {0}")]
    Zatoshis(#[from] zally_core::ZatoshisError),
    #[error("fixture error: {0}")]
    Fixture(String),
}

impl TestError {
    fn fixture(message: impl Into<String>) -> Self {
        Self::Fixture(message.into())
    }
}
