//! Profile-selectable payment-disclosure export from finalized PCZT material.

use pczt::roles::verifier::{
    OrchardError as PcztOrchardError, SaplingError as PcztSaplingError,
    TransparentError as PcztTransparentError, Verifier,
};
use rand::rngs::OsRng;
use zally_core::{FailurePosture, Network, PaymentRecipient, TxId, Zatoshis};
use zally_keys::SeedMaterial;
use zcash_keys::address::Address;
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_note_encryption::{EphemeralKeyBytes, try_output_recovery_with_ock};
use zcash_payment_disclosure::{
    IronwoodDisclosurePlan, IronwoodOutputSelection, IronwoodSpendSigningInput, PaymentDisclosure,
    PaymentDisclosureCodecError, PaymentDisclosurePlan, PaymentDisclosureProductionError,
    PaymentDisclosureProfile, SaplingOutputSelection, SaplingSpendProvingInput, prove_disclosure,
    sign_ironwood_disclosure,
};
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::consensus::Parameters as _;

use crate::PcztBytes;

/// Inputs for exporting one payment disclosure from a retained finalized PCZT.
#[derive(Clone, Debug)]
pub struct PaymentDisclosureExportPlan {
    transaction_id: TxId,
    recipient: PaymentRecipient,
    amount_zat: Zatoshis,
    message: Vec<u8>,
    profile: PaymentDisclosureProfile,
}

impl PaymentDisclosureExportPlan {
    /// Constructs a payment-disclosure export plan.
    #[must_use]
    pub fn new(
        transaction_id: TxId,
        recipient: PaymentRecipient,
        amount_zat: Zatoshis,
        message: Vec<u8>,
        profile: PaymentDisclosureProfile,
    ) -> Self {
        Self {
            transaction_id,
            recipient,
            amount_zat,
            message,
            profile,
        }
    }
}

/// Exports the selected payment-disclosure profile from one finalized PCZT and the owning seed.
///
/// Draft1 accepts only Sapling spend authority and creates fresh Sapling proofs. The Zally
/// Ironwood profile verifies retained Ironwood action metadata, then signs the disclosure with
/// the same randomized action keys that consensus bound to the mined nullifiers. Unsupported
/// pool combinations fail closed in both profiles.
///
/// # Errors
///
/// Returns a typed error for network mismatches, unsupported pools, incomplete or inconsistent
/// PCZT material, ambiguous output selection, missing parameters, key derivation failures, or
/// disclosure-production failures. Retrying the same inputs does not resolve any variant.
pub fn export_payment_disclosure(
    finalized_pczt: &PcztBytes,
    plan: PaymentDisclosureExportPlan,
    seed: &SeedMaterial,
) -> Result<PaymentDisclosure, PaymentDisclosureExportError> {
    let network = finalized_pczt.network();
    if plan.recipient.network() != network {
        return Err(PaymentDisclosureExportError::NetworkMismatch {
            pczt_network: network,
            recipient_network: plan.recipient.network(),
        });
    }
    let params = network.to_parameters();
    let spending_key =
        UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), zip32::AccountId::ZERO)
            .map_err(|err| PaymentDisclosureExportError::KeyDerivationFailed {
                reason: err.to_string(),
            })?;
    match plan.profile {
        PaymentDisclosureProfile::Zip311Draft1 => {
            let requested_recipient = Address::decode(&params, plan.recipient.encoded())
                .and_then(|address| address.to_sapling_address())
                .ok_or(PaymentDisclosureExportError::RecipientUnsupported)?;
            let verified_source =
                verify_sapling_disclosure_source(finalized_pczt, &plan, requested_recipient)?;
            let prover = LocalTxProver::with_default_location()
                .ok_or(PaymentDisclosureExportError::ProverUnavailable)?;
            let production_plan = PaymentDisclosurePlan::new(
                zcash_protocol::TxId::from_bytes(*plan.transaction_id.as_bytes()),
                params.network_type(),
                plan.message,
                verified_source.sapling_spends,
                vec![verified_source.selected_output],
            )?;
            prove_disclosure(
                production_plan,
                &spending_key.sapling().expsk.ask,
                &prover,
                &mut OsRng,
            )
            .map_err(Into::into)
        }
        PaymentDisclosureProfile::ZallyIronwood => {
            let requested_recipient = ironwood_recipient(&params, &plan.recipient)?;
            let ironwood_spending_key = Option::from(orchard::keys::SpendingKey::from_bytes(
                *spending_key.orchard().to_bytes(),
            ))
            .ok_or_else(|| PaymentDisclosureExportError::KeyDerivationFailed {
                reason: "Ironwood spending key bytes failed validation".into(),
            })?;
            let full_viewing_key = orchard::keys::FullViewingKey::from(&ironwood_spending_key);
            let outgoing_viewing_keys = [
                full_viewing_key.to_ovk(orchard::keys::Scope::External),
                full_viewing_key.to_ovk(orchard::keys::Scope::Internal),
            ];
            let verified_source = verify_ironwood_disclosure_source(
                finalized_pczt,
                &plan,
                requested_recipient,
                &outgoing_viewing_keys,
            )?;
            let ask = orchard::keys::SpendAuthorizingKey::from(&ironwood_spending_key);
            let production_plan = IronwoodDisclosurePlan::new(
                zcash_protocol::TxId::from_bytes(*plan.transaction_id.as_bytes()),
                params.network_type(),
                plan.message,
                verified_source.ironwood_spends,
                vec![verified_source.selected_output],
            )?;
            sign_ironwood_disclosure(production_plan, &ask, &mut OsRng).map_err(Into::into)
        }
        _ => Err(PaymentDisclosureExportError::ProfileUnsupported),
    }
}

struct VerifiedSaplingDisclosureSource {
    sapling_spends: Vec<SaplingSpendProvingInput>,
    selected_output: SaplingOutputSelection,
}

fn verify_sapling_disclosure_source(
    finalized_pczt: &PcztBytes,
    plan: &PaymentDisclosureExportPlan,
    requested_sapling_recipient: sapling::PaymentAddress,
) -> Result<VerifiedSaplingDisclosureSource, PaymentDisclosureExportError> {
    let parsed =
        finalized_pczt
            .parse()
            .map_err(|err| PaymentDisclosureExportError::PcztMalformed {
                reason: err.to_string(),
            })?;
    let mut sapling_spends = Vec::new();
    let mut matching_outputs = Vec::new();
    let verifier = Verifier::new(parsed)
        .with_transparent::<PaymentDisclosureExportError, _>(|bundle| {
            if bundle.inputs().is_empty() {
                Ok(())
            } else {
                Err(PcztTransparentError::Custom(
                    PaymentDisclosureExportError::TransparentInputsUnsupported,
                ))
            }
        })
        .map_err(map_transparent_error)?
        .with_orchard::<PaymentDisclosureExportError, _>(|bundle| {
            if bundle.actions().is_empty() {
                Ok(())
            } else {
                Err(PcztOrchardError::Custom(
                    PaymentDisclosureExportError::OrchardActionsUnsupported,
                ))
            }
        })
        .map_err(|err| map_orchard_error("Orchard", err))?
        .with_ironwood::<PaymentDisclosureExportError, _>(|bundle| {
            if bundle.actions().is_empty() {
                Ok(())
            } else {
                Err(PcztOrchardError::Custom(
                    PaymentDisclosureExportError::IronwoodActionsUnsupported,
                ))
            }
        })
        .map_err(|err| map_orchard_error("Ironwood", err))?
        .with_sapling::<PaymentDisclosureExportError, _>(|bundle| {
            collect_sapling_spends(bundle, &mut sapling_spends)?;
            collect_matching_sapling_outputs(
                bundle,
                plan.recipient.encoded(),
                requested_sapling_recipient,
                plan.amount_zat.as_u64(),
                &mut matching_outputs,
            )?;
            Ok(())
        })
        .map_err(map_sapling_error)?;
    let _ = verifier.finish();

    if sapling_spends.is_empty() {
        return Err(PaymentDisclosureExportError::SaplingSpendsMissing);
    }
    let selected_output = match matching_outputs.as_slice() {
        [selected_output] => *selected_output,
        [] => return Err(PaymentDisclosureExportError::SaplingOutputNotFound),
        _ => return Err(PaymentDisclosureExportError::SaplingOutputAmbiguous),
    };

    Ok(VerifiedSaplingDisclosureSource {
        sapling_spends,
        selected_output,
    })
}

struct VerifiedIronwoodDisclosureSource {
    ironwood_spends: Vec<IronwoodSpendSigningInput>,
    selected_output: IronwoodOutputSelection,
}

fn ironwood_recipient(
    params: &zally_core::NetworkParameters,
    recipient: &PaymentRecipient,
) -> Result<orchard::Address, PaymentDisclosureExportError> {
    let decoded = Address::decode(params, recipient.encoded())
        .ok_or(PaymentDisclosureExportError::RecipientUnsupported)?;
    match decoded {
        Address::Unified(unified) => unified
            .orchard()
            .copied()
            .ok_or(PaymentDisclosureExportError::RecipientUnsupported),
        Address::Sapling(_) | Address::Transparent(_) | Address::Tex(_) => {
            Err(PaymentDisclosureExportError::RecipientUnsupported)
        }
    }
}

fn verify_ironwood_disclosure_source(
    finalized_pczt: &PcztBytes,
    plan: &PaymentDisclosureExportPlan,
    requested_ironwood_recipient: orchard::Address,
    outgoing_viewing_keys: &[orchard::keys::OutgoingViewingKey],
) -> Result<VerifiedIronwoodDisclosureSource, PaymentDisclosureExportError> {
    let parsed =
        finalized_pczt
            .parse()
            .map_err(|err| PaymentDisclosureExportError::PcztMalformed {
                reason: err.to_string(),
            })?;
    let mut ironwood_spends = Vec::new();
    let mut matching_outputs = Vec::new();
    let output_expectation = IronwoodOutputExpectation {
        encoded_recipient: plan.recipient.encoded(),
        ironwood_recipient: requested_ironwood_recipient,
        amount_zat: plan.amount_zat.as_u64(),
        outgoing_viewing_keys,
    };
    let verifier = Verifier::new(parsed)
        .with_transparent::<PaymentDisclosureExportError, _>(|bundle| {
            if bundle.inputs().is_empty() {
                Ok(())
            } else {
                Err(PcztTransparentError::Custom(
                    PaymentDisclosureExportError::TransparentInputsUnsupported,
                ))
            }
        })
        .map_err(map_transparent_error)?
        .with_sapling::<PaymentDisclosureExportError, _>(|bundle| {
            if bundle.spends().is_empty() && bundle.outputs().is_empty() {
                Ok(())
            } else {
                Err(PcztSaplingError::Custom(
                    PaymentDisclosureExportError::SaplingActionsUnsupported,
                ))
            }
        })
        .map_err(map_sapling_error)?
        .with_orchard::<PaymentDisclosureExportError, _>(|bundle| {
            if bundle.actions().is_empty() {
                Ok(())
            } else {
                Err(PcztOrchardError::Custom(
                    PaymentDisclosureExportError::OrchardActionsUnsupported,
                ))
            }
        })
        .map_err(|err| map_orchard_error("Orchard", err))?
        .with_ironwood::<PaymentDisclosureExportError, _>(|bundle| {
            collect_ironwood_spends(bundle, &mut ironwood_spends)?;
            collect_matching_ironwood_outputs(bundle, &output_expectation, &mut matching_outputs)?;
            Ok(())
        })
        .map_err(|err| map_orchard_error("Ironwood", err))?;
    let _ = verifier.finish();

    if ironwood_spends.is_empty() {
        return Err(PaymentDisclosureExportError::IronwoodSpendsMissing);
    }
    let selected_output = match matching_outputs.as_slice() {
        [selected_output] => *selected_output,
        [] => return Err(PaymentDisclosureExportError::IronwoodOutputNotFound),
        _ => return Err(PaymentDisclosureExportError::IronwoodOutputAmbiguous),
    };
    Ok(VerifiedIronwoodDisclosureSource {
        ironwood_spends,
        selected_output,
    })
}

fn collect_ironwood_spends(
    bundle: &orchard::pczt::Bundle,
    ironwood_spends: &mut Vec<IronwoodSpendSigningInput>,
) -> Result<(), PcztOrchardError<PaymentDisclosureExportError>> {
    for (index, action) in bundle.actions().iter().enumerate() {
        let has_spend_amount = action
            .spend()
            .value()
            .as_ref()
            .is_some_and(|amount| amount.inner() > 0);
        if !has_spend_amount {
            continue;
        }
        let index = u32::try_from(index).map_err(|_| {
            PcztOrchardError::Custom(PaymentDisclosureExportError::IronwoodActionIndexOutOfRange)
        })?;
        action.spend().verify_nullifier(None)?;
        action.spend().verify_rk(None)?;
        let alpha = action
            .spend()
            .alpha()
            .as_ref()
            .copied()
            .ok_or(PcztOrchardError::Custom(
                PaymentDisclosureExportError::IronwoodSpendIncomplete {
                    index,
                    field: "alpha",
                },
            ))?;
        ironwood_spends.push(IronwoodSpendSigningInput::new(
            index,
            alpha,
            action.spend().rk().clone(),
        ));
    }
    Ok(())
}

struct IronwoodOutputExpectation<'a> {
    encoded_recipient: &'a str,
    ironwood_recipient: orchard::Address,
    amount_zat: u64,
    outgoing_viewing_keys: &'a [orchard::keys::OutgoingViewingKey],
}

fn collect_matching_ironwood_outputs(
    bundle: &orchard::pczt::Bundle,
    expectation: &IronwoodOutputExpectation<'_>,
    matching_outputs: &mut Vec<IronwoodOutputSelection>,
) -> Result<(), PcztOrchardError<PaymentDisclosureExportError>> {
    for (index, action) in bundle.actions().iter().enumerate() {
        let output = action.output();
        let has_requested_recipient =
            output.user_address().as_deref() == Some(expectation.encoded_recipient);
        let has_requested_ironwood_recipient =
            output.recipient().as_ref() == Some(&expectation.ironwood_recipient);
        let has_requested_amount = output
            .value()
            .as_ref()
            .is_some_and(|amount| amount.inner() == expectation.amount_zat);
        if has_requested_recipient && has_requested_ironwood_recipient && has_requested_amount {
            let index = u32::try_from(index).map_err(|_| {
                PcztOrchardError::Custom(
                    PaymentDisclosureExportError::IronwoodActionIndexOutOfRange,
                )
            })?;
            let ock = derive_ironwood_output_ock(action, expectation.outgoing_viewing_keys).ok_or(
                PcztOrchardError::Custom(PaymentDisclosureExportError::IronwoodOutputOckMissing {
                    index,
                }),
            )?;
            matching_outputs.push(IronwoodOutputSelection::new(index, ock));
        }
    }
    Ok(())
}

fn derive_ironwood_output_ock(
    action: &orchard::pczt::Action,
    outgoing_viewing_keys: &[orchard::keys::OutgoingViewingKey],
) -> Option<[u8; 32]> {
    if let Some(ock) = action.output().ock().as_ref() {
        return Some(ock.0);
    }
    let domain = orchard::note_encryption::IronwoodDomain::for_pczt_action(action);
    let ephemeral_key = EphemeralKeyBytes(action.output().encrypted_note().epk_bytes);
    for outgoing_viewing_key in outgoing_viewing_keys {
        let ock =
            <orchard::note_encryption::IronwoodDomain as zcash_note_encryption::Domain>::derive_ock(
                outgoing_viewing_key,
                action.cv_net(),
                &action.output().cmx().to_bytes(),
                &ephemeral_key,
            );
        if try_output_recovery_with_ock(
            &domain,
            &ock,
            action,
            &action.output().encrypted_note().out_ciphertext,
        )
        .is_some()
        {
            return Some(ock.0);
        }
    }
    None
}

fn collect_sapling_spends(
    bundle: &sapling::pczt::Bundle,
    sapling_spends: &mut Vec<SaplingSpendProvingInput>,
) -> Result<(), PcztSaplingError<PaymentDisclosureExportError>> {
    let anchor = *bundle.anchor();
    for (index, spend) in bundle.spends().iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| {
            PcztSaplingError::Custom(PaymentDisclosureExportError::SaplingSpendIndexOutOfRange)
        })?;
        spend.verify_nullifier(None)?;
        let recipient = spend
            .recipient()
            .as_ref()
            .copied()
            .ok_or_else(|| incomplete_sapling_spend(index, "recipient"))?;
        let amount_zat = spend
            .value()
            .as_ref()
            .map(sapling::value::NoteValue::inner)
            .ok_or_else(|| incomplete_sapling_spend(index, "value_zat"))?;
        let rseed = spend
            .rseed()
            .as_ref()
            .copied()
            .ok_or_else(|| incomplete_sapling_spend(index, "rseed"))?;
        let proof_generation_key = spend
            .proof_generation_key()
            .clone()
            .ok_or_else(|| incomplete_sapling_spend(index, "proof_generation_key"))?;
        let witness = spend
            .witness()
            .clone()
            .ok_or_else(|| incomplete_sapling_spend(index, "witness"))?;
        sapling_spends.push(SaplingSpendProvingInput::new(
            index,
            recipient,
            amount_zat,
            rseed,
            proof_generation_key,
            anchor,
            witness,
        ));
    }
    Ok(())
}

fn collect_matching_sapling_outputs(
    bundle: &sapling::pczt::Bundle,
    requested_recipient: &str,
    requested_sapling_recipient: sapling::PaymentAddress,
    requested_amount_zat: u64,
    matching_outputs: &mut Vec<SaplingOutputSelection>,
) -> Result<(), PcztSaplingError<PaymentDisclosureExportError>> {
    for (index, output) in bundle.outputs().iter().enumerate() {
        let has_requested_recipient = output.user_address().as_deref() == Some(requested_recipient);
        let has_requested_sapling_recipient =
            output.recipient().as_ref() == Some(&requested_sapling_recipient);
        let has_requested_amount = output
            .value()
            .as_ref()
            .is_some_and(|amount| amount.inner() == requested_amount_zat);
        if has_requested_recipient && has_requested_sapling_recipient && has_requested_amount {
            let index = u32::try_from(index).map_err(|_| {
                PcztSaplingError::Custom(PaymentDisclosureExportError::SaplingOutputIndexOutOfRange)
            })?;
            let ock = output.ock().as_ref().ok_or(PcztSaplingError::Custom(
                PaymentDisclosureExportError::SaplingOutputOckMissing { index },
            ))?;
            matching_outputs.push(SaplingOutputSelection::new(index, ock.0));
        }
    }
    Ok(())
}

fn incomplete_sapling_spend(
    index: u32,
    field: &'static str,
) -> PcztSaplingError<PaymentDisclosureExportError> {
    PcztSaplingError::Custom(PaymentDisclosureExportError::SaplingSpendIncomplete { index, field })
}

fn map_transparent_error(
    error: PcztTransparentError<PaymentDisclosureExportError>,
) -> PaymentDisclosureExportError {
    match error {
        PcztTransparentError::Custom(error) => error,
        other @ (PcztTransparentError::Parser(_) | PcztTransparentError::Verifier(_)) => {
            PaymentDisclosureExportError::PcztSectionMalformed {
                section: "transparent",
                reason: format!("{other:?}"),
            }
        }
    }
}

fn map_orchard_error(
    section: &'static str,
    error: PcztOrchardError<PaymentDisclosureExportError>,
) -> PaymentDisclosureExportError {
    match error {
        PcztOrchardError::Custom(error) => error,
        other @ (PcztOrchardError::Parse(_)
        | PcztOrchardError::UnsupportedConsensusBranchId
        | PcztOrchardError::Verify(_)) => PaymentDisclosureExportError::PcztSectionMalformed {
            section,
            reason: format!("{other:?}"),
        },
    }
}

fn map_sapling_error(
    error: PcztSaplingError<PaymentDisclosureExportError>,
) -> PaymentDisclosureExportError {
    match error {
        PcztSaplingError::Custom(error) => error,
        PcztSaplingError::Verifier(source) => PaymentDisclosureExportError::SaplingSpendInvalid {
            reason: format!("{source:?}"),
        },
        other @ PcztSaplingError::Parser(_) => PaymentDisclosureExportError::PcztSectionMalformed {
            section: "Sapling",
            reason: format!("{other:?}"),
        },
    }
}

/// Failure to export a payment disclosure from a finalized PCZT.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PaymentDisclosureExportError {
    /// Recipient and PCZT networks disagree. Retry posture: `requires_operator`.
    #[error(
        "payment-disclosure network mismatch: pczt={pczt_network:?}, recipient={recipient_network:?}"
    )]
    NetworkMismatch {
        /// Network carried by the retained PCZT.
        pczt_network: Network,
        /// Network carried by the requested recipient.
        recipient_network: Network,
    },
    /// The recipient has no receiver supported by the selected profile. Retry posture: `not_retryable`.
    #[error("payment-disclosure recipient is unsupported by the selected profile")]
    RecipientUnsupported,
    /// The requested profile is not implemented. Retry posture: `not_retryable`.
    #[error("payment-disclosure profile is unsupported by this release")]
    ProfileUnsupported,
    /// The retained PCZT cannot be parsed. Retry posture: `requires_operator`.
    #[error("retained finalized PCZT is malformed: {reason}")]
    PcztMalformed {
        /// Decoder failure without sensitive PCZT contents.
        reason: String,
    },
    /// One protocol section cannot be parsed. Retry posture: `requires_operator`.
    #[error("retained finalized PCZT {section} section is malformed: {reason}")]
    PcztSectionMalformed {
        /// Protocol section that failed.
        section: &'static str,
        /// Parser failure without sensitive PCZT contents.
        reason: String,
    },
    /// Draft1 refuses transparent authority. Retry posture: `not_retryable`.
    #[error("ZIP-311 Draft1 export does not support transparent inputs")]
    TransparentInputsUnsupported,
    /// The Ironwood profile refuses Sapling sections. Retry posture: `not_retryable`.
    #[error("the Zally Ironwood disclosure profile does not support Sapling sections")]
    SaplingActionsUnsupported,
    /// Draft1 refuses Orchard actions. Retry posture: `not_retryable`.
    #[error("ZIP-311 Draft1 export does not support Orchard actions")]
    OrchardActionsUnsupported,
    /// Draft1 refuses Ironwood actions. Retry posture: `not_retryable`.
    #[error("ZIP-311 Draft1 export does not support Ironwood actions")]
    IronwoodActionsUnsupported,
    /// An Ironwood spend failed its retained-metadata checks. Retry posture: `requires_operator`.
    #[error("retained Ironwood spend is inconsistent: {reason}")]
    IronwoodSpendInvalid {
        /// Verification failure without sensitive note contents.
        reason: String,
    },
    /// An Ironwood spend lacks retained signing material. Retry posture: `requires_operator`.
    #[error("Ironwood action {index} is missing retained {field}")]
    IronwoodSpendIncomplete {
        /// Action index.
        index: u32,
        /// Missing PCZT field.
        field: &'static str,
    },
    /// The PCZT has more Ironwood actions than the extension can index. Retry posture: `not_retryable`.
    #[error("Ironwood action index exceeds the disclosure u32 range")]
    IronwoodActionIndexOutOfRange,
    /// The PCZT proves no Ironwood spend authority. Retry posture: `not_retryable`.
    #[error("the Zally Ironwood disclosure profile requires at least one Ironwood spend")]
    IronwoodSpendsMissing,
    /// No Ironwood output matches recipient and amount. Retry posture: `not_retryable`.
    #[error("no Ironwood output matches the requested recipient and amount")]
    IronwoodOutputNotFound,
    /// Multiple Ironwood outputs match recipient and amount. Retry posture: `not_retryable`.
    #[error("multiple Ironwood outputs match the requested recipient and amount")]
    IronwoodOutputAmbiguous,
    /// The selected Ironwood output omitted its OCK. Retry posture: `requires_operator`.
    #[error("Ironwood action {index} output has no retained outgoing cipher key")]
    IronwoodOutputOckMissing {
        /// Action index.
        index: u32,
    },
    /// A Sapling spend failed its nullifier check. Retry posture: `requires_operator`.
    #[error("retained Sapling spend is inconsistent: {reason}")]
    SaplingSpendInvalid {
        /// Verification failure without sensitive note contents.
        reason: String,
    },
    /// A Sapling spend lacks retained proving material. Retry posture: `requires_operator`.
    #[error("Sapling spend {index} is missing retained {field}")]
    SaplingSpendIncomplete {
        /// Spend index.
        index: u32,
        /// Missing PCZT field.
        field: &'static str,
    },
    /// The PCZT contains more Sapling spends than Draft1 can index. Retry posture: `not_retryable`.
    #[error("Sapling spend index exceeds the Draft1 u32 range")]
    SaplingSpendIndexOutOfRange,
    /// The PCZT proves no Sapling spend authority. Retry posture: `not_retryable`.
    #[error("ZIP-311 Draft1 export requires at least one Sapling spend")]
    SaplingSpendsMissing,
    /// No Sapling output matches recipient and amount. Retry posture: `not_retryable`.
    #[error("no Sapling output matches the requested recipient and amount")]
    SaplingOutputNotFound,
    /// Multiple Sapling outputs match recipient and amount. Retry posture: `not_retryable`.
    #[error("multiple Sapling outputs match the requested recipient and amount")]
    SaplingOutputAmbiguous,
    /// The selected output omitted its OCK. Retry posture: `requires_operator`.
    #[error("Sapling output {index} has no retained outgoing cipher key")]
    SaplingOutputOckMissing {
        /// Output index.
        index: u32,
    },
    /// The PCZT contains more Sapling outputs than Draft1 can index. Retry posture: `not_retryable`.
    #[error("Sapling output index exceeds the Draft1 u32 range")]
    SaplingOutputIndexOutOfRange,
    /// ZIP-32 key derivation failed. Retry posture: `requires_operator`.
    #[error("payment-disclosure key derivation failed: {reason}")]
    KeyDerivationFailed {
        /// Derivation failure without seed material.
        reason: String,
    },
    /// Sapling proving parameters are unavailable. Retry posture: `requires_operator`.
    #[error("Sapling proving parameters are unavailable for payment-disclosure export")]
    ProverUnavailable,
    /// The standalone codec rejected the selected profile's plan. Retry posture: `not_retryable`.
    #[error("payment-disclosure plan is invalid: {0}")]
    Codec(#[from] PaymentDisclosureCodecError),
    /// Profile-specific proof or signature production failed. Retry posture depends on the source variant.
    #[error("payment-disclosure production failed: {0}")]
    Production(#[from] PaymentDisclosureProductionError),
}

impl PaymentDisclosureExportError {
    /// Operator-facing posture describing what the consumer may do.
    #[must_use]
    pub const fn posture(&self) -> FailurePosture {
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "the production error is non-exhaustive across the standalone crate boundary"
        )]
        match self {
            Self::Production(source) => match source {
                PaymentDisclosureProductionError::IronwoodSpendAuthorizingKeyMismatch {
                    ..
                }
                | PaymentDisclosureProductionError::SpendAuthorizingKeyMismatch { .. }
                | PaymentDisclosureProductionError::SpendCircuitInvalid { .. }
                | PaymentDisclosureProductionError::Codec(_) => FailurePosture::NotRetryable,
                PaymentDisclosureProductionError::IronwoodSpendSignatureInvalid { .. }
                | PaymentDisclosureProductionError::RandomizedVerificationKeyMismatch { .. }
                | PaymentDisclosureProductionError::SpendSignatureInvalid { .. } => {
                    FailurePosture::RequiresOperator
                }
                _ => FailurePosture::RequiresOperator,
            },
            Self::NetworkMismatch { .. }
            | Self::PcztMalformed { .. }
            | Self::PcztSectionMalformed { .. }
            | Self::IronwoodSpendInvalid { .. }
            | Self::IronwoodSpendIncomplete { .. }
            | Self::IronwoodOutputOckMissing { .. }
            | Self::SaplingSpendInvalid { .. }
            | Self::SaplingSpendIncomplete { .. }
            | Self::SaplingOutputOckMissing { .. }
            | Self::KeyDerivationFailed { .. }
            | Self::ProverUnavailable => FailurePosture::RequiresOperator,
            Self::ProfileUnsupported
            | Self::RecipientUnsupported
            | Self::TransparentInputsUnsupported
            | Self::SaplingActionsUnsupported
            | Self::OrchardActionsUnsupported
            | Self::IronwoodActionsUnsupported
            | Self::IronwoodActionIndexOutOfRange
            | Self::IronwoodSpendsMissing
            | Self::IronwoodOutputNotFound
            | Self::IronwoodOutputAmbiguous
            | Self::SaplingSpendIndexOutOfRange
            | Self::SaplingSpendsMissing
            | Self::SaplingOutputNotFound
            | Self::SaplingOutputAmbiguous
            | Self::SaplingOutputIndexOutOfRange
            | Self::Codec(_) => FailurePosture::NotRetryable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_postures_follow_the_source_variant() {
        let caller_error = PaymentDisclosureExportError::Production(
            PaymentDisclosureProductionError::SpendCircuitInvalid { index: 0 },
        );
        let internal_error = PaymentDisclosureExportError::Production(
            PaymentDisclosureProductionError::RandomizedVerificationKeyMismatch { index: 0 },
        );

        assert_eq!(caller_error.posture(), FailurePosture::NotRetryable);
        assert_eq!(internal_error.posture(), FailurePosture::RequiresOperator);
    }
}
