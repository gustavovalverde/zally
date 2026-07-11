use std::{fmt, io::Cursor};

use bellman::groth16::Proof;
use bls12_381::Bls12;
use orchard::{
    Address as IronwoodAddress,
    note_encryption::IronwoodDomain,
    primitives::redpallas::{Signature as IronwoodSignature, SpendAuth as IronwoodSpendAuth},
};
use redjubjub::{Signature, SpendAuth, VerificationKey};
use sapling::{
    PaymentAddress, SaplingVerificationContext,
    bundle::{Authorized, GrothProofBytes, OutputDescription, SpendDescription},
    circuit::PreparedSpendVerifyingKey,
    note_encryption::{Zip212Enforcement, try_sapling_output_recovery_with_ock},
    value::ValueCommitment,
};
use zcash_note_encryption::{OutgoingCipherKey, try_output_recovery_with_ock};
use zcash_primitives::transaction::{Transaction, components::sapling::zip212_enforcement};
use zcash_protocol::{
    TxId,
    consensus::{BlockHeight, BranchId, Parameters},
};

use crate::{PaymentDisclosure, PaymentDisclosureProfile};

/// Evidence that every selected spend and output disclosure verified for its profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentDisclosureEvidence {
    transaction_id: TxId,
    sapling_spends: Vec<SaplingSpendEvidence>,
    sapling_outputs: Vec<SaplingOutputEvidence>,
    ironwood_spends: Vec<IronwoodSpendEvidence>,
    ironwood_outputs: Vec<IronwoodOutputEvidence>,
}

impl PaymentDisclosureEvidence {
    /// Returns the verified transaction identifier.
    #[must_use]
    pub const fn transaction_id(&self) -> TxId {
        self.transaction_id
    }

    /// Returns the Sapling spends whose authority proofs verified.
    #[must_use]
    pub fn sapling_spends(&self) -> &[SaplingSpendEvidence] {
        &self.sapling_spends
    }

    /// Returns the Sapling outputs recovered by their disclosed outgoing cipher keys.
    #[must_use]
    pub fn sapling_outputs(&self) -> &[SaplingOutputEvidence] {
        &self.sapling_outputs
    }

    /// Returns the Ironwood actions whose message-bound authority signatures verified.
    #[must_use]
    pub fn ironwood_spends(&self) -> &[IronwoodSpendEvidence] {
        &self.ironwood_spends
    }

    /// Returns the Ironwood outputs recovered by their disclosed outgoing cipher keys.
    #[must_use]
    pub fn ironwood_outputs(&self) -> &[IronwoodOutputEvidence] {
        &self.ironwood_outputs
    }
}

/// Evidence that a selected Sapling spend proves authority over its on-chain nullifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SaplingSpendEvidence {
    index: u32,
}

impl SaplingSpendEvidence {
    /// Returns the verified index in the transaction's Sapling spend sequence.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }
}

/// Recovered facts for one selected Sapling output.
#[derive(Clone, Eq, PartialEq)]
pub struct SaplingOutputEvidence {
    index: u32,
    recipient: PaymentAddress,
    amount_zat: u64,
    memo: [u8; 512],
}

impl fmt::Debug for SaplingOutputEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SaplingOutputEvidence")
            .field("index", &self.index)
            .field("recipient", &self.recipient)
            .field("amount_zat", &self.amount_zat)
            .field("memo_bytes", &self.memo.len())
            .finish()
    }
}

impl SaplingOutputEvidence {
    /// Returns the verified index in the transaction's Sapling output sequence.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// Returns the recovered Sapling recipient.
    #[must_use]
    pub const fn recipient(&self) -> PaymentAddress {
        self.recipient
    }

    /// Returns the recovered note amount in zatoshis.
    #[must_use]
    pub const fn amount_zat(&self) -> u64 {
        self.amount_zat
    }

    /// Returns the recovered 512-byte memo field.
    #[must_use]
    pub const fn memo(&self) -> &[u8; 512] {
        &self.memo
    }
}

/// Evidence that a mined Ironwood action accepted the disclosed authority signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IronwoodSpendEvidence {
    index: u32,
}

impl IronwoodSpendEvidence {
    /// Returns the verified Ironwood action index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }
}

/// Recovered facts for one selected Ironwood output.
#[derive(Clone, Eq, PartialEq)]
pub struct IronwoodOutputEvidence {
    index: u32,
    recipient: IronwoodAddress,
    amount_zat: u64,
    memo: [u8; 512],
}

impl fmt::Debug for IronwoodOutputEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IronwoodOutputEvidence")
            .field("index", &self.index)
            .field("recipient", &self.recipient)
            .field("amount_zat", &self.amount_zat)
            .field("memo_bytes", &self.memo.len())
            .finish()
    }
}

impl IronwoodOutputEvidence {
    /// Returns the verified action index in the transaction's Ironwood bundle.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// Returns the recovered Ironwood recipient.
    #[must_use]
    pub const fn recipient(&self) -> IronwoodAddress {
        self.recipient
    }

    /// Returns the recovered note amount in zatoshis.
    #[must_use]
    pub const fn amount_zat(&self) -> u64 {
        self.amount_zat
    }

    /// Returns the recovered 512-byte memo field.
    #[must_use]
    pub const fn memo(&self) -> &[u8; 512] {
        &self.memo
    }
}

/// Verifies a payment disclosure against the exact mined transaction it references.
///
/// The caller supplies the network consensus parameters, mined height, raw transaction,
/// and prepared Sapling Spend verifying key. This function performs no chain fetching or
/// proving-parameter loading.
///
/// # Errors
///
/// Returns an error when the transaction is malformed or mismatched, a selected index is
/// absent, a spend authority proof is invalid, or an output cannot be recovered by its OCK.
pub fn verify_disclosure<ParamsT: Parameters>(
    disclosure: &PaymentDisclosure,
    transaction_bytes: &[u8],
    mined_height: BlockHeight,
    params: &ParamsT,
    spend_verifying_key: &PreparedSpendVerifyingKey,
) -> Result<PaymentDisclosureEvidence, PaymentDisclosureVerificationError> {
    let transaction = parse_transaction(transaction_bytes, mined_height, params)?;
    let actual_transaction_id = transaction.txid();
    let expected_transaction_id = disclosure.transaction_id();
    if actual_transaction_id != expected_transaction_id {
        return Err(PaymentDisclosureVerificationError::TransactionIdMismatch {
            expected_transaction_id,
            actual_transaction_id,
        });
    }

    let disclosure_digest = disclosure.compute_digest(params.network_type());
    let (sapling_spends, sapling_outputs, ironwood_spends, ironwood_outputs) = match disclosure
        .profile()
    {
        PaymentDisclosureProfile::Zip311Draft1 => {
            let sapling_bundle = transaction
                .sapling_bundle()
                .ok_or(PaymentDisclosureVerificationError::SaplingBundleMissing)?;
            (
                verify_sapling_spends(
                    disclosure,
                    sapling_bundle.shielded_spends(),
                    disclosure_digest,
                    spend_verifying_key,
                )?,
                recover_sapling_outputs(
                    disclosure,
                    sapling_bundle.shielded_outputs(),
                    zip212_enforcement(params, mined_height),
                )?,
                Vec::new(),
                Vec::new(),
            )
        }
        PaymentDisclosureProfile::ZallyIronwood => {
            let ironwood_bundle = transaction
                .ironwood_bundle()
                .ok_or(PaymentDisclosureVerificationError::IronwoodBundleMissing)?;
            (
                Vec::new(),
                Vec::new(),
                verify_ironwood_spends(disclosure, ironwood_bundle.actions(), disclosure_digest)?,
                recover_ironwood_outputs(disclosure, ironwood_bundle.actions())?,
            )
        }
    };

    Ok(PaymentDisclosureEvidence {
        transaction_id: actual_transaction_id,
        sapling_spends,
        sapling_outputs,
        ironwood_spends,
        ironwood_outputs,
    })
}

fn verify_sapling_spends(
    disclosure: &PaymentDisclosure,
    on_chain_spends: &[SpendDescription<Authorized>],
    disclosure_digest: [u8; 32],
    spend_verifying_key: &PreparedSpendVerifyingKey,
) -> Result<Vec<SaplingSpendEvidence>, PaymentDisclosureVerificationError> {
    let mut verified_spends = Vec::with_capacity(disclosure.sapling_spends().len());
    for disclosed_spend in disclosure.sapling_spends() {
        let index = disclosed_spend.index();
        let on_chain_spend = on_chain_spends.get(index as usize).ok_or({
            PaymentDisclosureVerificationError::SpendIndexOutOfBounds {
                index,
                spend_count: on_chain_spends.len(),
            }
        })?;
        let cv = Option::from(ValueCommitment::from_bytes_not_small_order(
            disclosed_spend.cv_bytes(),
        ))
        .ok_or(PaymentDisclosureVerificationError::SpendCommitmentMalformed { index })?;
        let rk = VerificationKey::<SpendAuth>::try_from(disclosed_spend.rk_bytes()).map_err(
            |source| PaymentDisclosureVerificationError::RandomizedVerificationKeyMalformed {
                index,
                source,
            },
        )?;
        let zkproof =
            Proof::<Bls12>::read(&disclosed_spend.zkproof_bytes()[..]).map_err(|source| {
                PaymentDisclosureVerificationError::SpendProofMalformed { index, source }
            })?;
        let spend_auth_sig = Signature::<SpendAuth>::from(disclosed_spend.spend_auth_sig_bytes());
        let mut verification_context = SaplingVerificationContext::new();
        if !verification_context.check_spend(
            &cv,
            *on_chain_spend.anchor(),
            &on_chain_spend.nullifier().0,
            rk,
            &disclosure_digest,
            spend_auth_sig,
            zkproof,
            spend_verifying_key,
        ) {
            return Err(PaymentDisclosureVerificationError::SpendAuthorityInvalid { index });
        }
        verified_spends.push(SaplingSpendEvidence { index });
    }
    Ok(verified_spends)
}

fn recover_sapling_outputs(
    disclosure: &PaymentDisclosure,
    on_chain_outputs: &[OutputDescription<GrothProofBytes>],
    enforcement: Zip212Enforcement,
) -> Result<Vec<SaplingOutputEvidence>, PaymentDisclosureVerificationError> {
    let mut recovered_outputs = Vec::with_capacity(disclosure.sapling_outputs().len());
    for disclosed_output in disclosure.sapling_outputs() {
        let index = disclosed_output.index();
        let on_chain_output = on_chain_outputs.get(index as usize).ok_or({
            PaymentDisclosureVerificationError::OutputIndexOutOfBounds {
                index,
                output_count: on_chain_outputs.len(),
            }
        })?;
        let ock = OutgoingCipherKey(disclosed_output.ock_bytes());
        let (note, recipient, memo) =
            try_sapling_output_recovery_with_ock(&ock, on_chain_output, enforcement)
                .ok_or(PaymentDisclosureVerificationError::OutputRecoveryFailed { index })?;
        recovered_outputs.push(SaplingOutputEvidence {
            index,
            recipient,
            amount_zat: note.value().inner(),
            memo,
        });
    }

    Ok(recovered_outputs)
}

fn verify_ironwood_spends(
    disclosure: &PaymentDisclosure,
    on_chain_actions: &nonempty::NonEmpty<orchard::Action<IronwoodSignature<IronwoodSpendAuth>>>,
    disclosure_digest: [u8; 32],
) -> Result<Vec<IronwoodSpendEvidence>, PaymentDisclosureVerificationError> {
    let mut verified_spends = Vec::with_capacity(disclosure.ironwood_spends().len());
    for disclosed_spend in disclosure.ironwood_spends() {
        let index = disclosed_spend.index();
        let on_chain_action = on_chain_actions.get(index as usize).ok_or(
            PaymentDisclosureVerificationError::IronwoodActionIndexOutOfBounds {
                index,
                action_count: on_chain_actions.len(),
            },
        )?;
        let spend_auth_sig =
            IronwoodSignature::<IronwoodSpendAuth>::from(disclosed_spend.spend_auth_sig_bytes());
        if on_chain_action
            .rk()
            .verify(&disclosure_digest, &spend_auth_sig)
            .is_err()
        {
            return Err(
                PaymentDisclosureVerificationError::IronwoodSpendAuthorityInvalid { index },
            );
        }
        verified_spends.push(IronwoodSpendEvidence { index });
    }
    Ok(verified_spends)
}

fn recover_ironwood_outputs(
    disclosure: &PaymentDisclosure,
    on_chain_actions: &nonempty::NonEmpty<orchard::Action<IronwoodSignature<IronwoodSpendAuth>>>,
) -> Result<Vec<IronwoodOutputEvidence>, PaymentDisclosureVerificationError> {
    let mut recovered_outputs = Vec::with_capacity(disclosure.ironwood_outputs().len());
    for disclosed_output in disclosure.ironwood_outputs() {
        let index = disclosed_output.index();
        let on_chain_action = on_chain_actions.get(index as usize).ok_or(
            PaymentDisclosureVerificationError::IronwoodActionIndexOutOfBounds {
                index,
                action_count: on_chain_actions.len(),
            },
        )?;
        let domain = IronwoodDomain::for_action(on_chain_action);
        let ock = OutgoingCipherKey(disclosed_output.ock_bytes());
        let (note, recipient, memo) = try_output_recovery_with_ock(
            &domain,
            &ock,
            on_chain_action,
            &on_chain_action.encrypted_note().out_ciphertext,
        )
        .ok_or(PaymentDisclosureVerificationError::IronwoodOutputRecoveryFailed { index })?;
        recovered_outputs.push(IronwoodOutputEvidence {
            index,
            recipient,
            amount_zat: note.value().inner(),
            memo,
        });
    }
    Ok(recovered_outputs)
}

fn parse_transaction<ParamsT: Parameters>(
    transaction_bytes: &[u8],
    mined_height: BlockHeight,
    params: &ParamsT,
) -> Result<Transaction, PaymentDisclosureVerificationError> {
    let mut transaction_cursor = Cursor::new(transaction_bytes);
    let transaction = Transaction::read(
        &mut transaction_cursor,
        BranchId::for_height(params, mined_height),
    )
    .map_err(|source| PaymentDisclosureVerificationError::TransactionMalformed { source })?;
    let consumed_bytes =
        usize::try_from(transaction_cursor.position()).unwrap_or(transaction_bytes.len());
    if consumed_bytes != transaction_bytes.len() {
        return Err(
            PaymentDisclosureVerificationError::TransactionTrailingBytes {
                trailing_bytes: transaction_bytes.len() - consumed_bytes,
            },
        );
    }
    Ok(transaction)
}

/// Failure to verify a payment disclosure against its mined transaction.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PaymentDisclosureVerificationError {
    /// The transaction bytes cannot be decoded. Retry posture: `not_retryable`.
    #[error("disclosed transaction is malformed: {source}")]
    TransactionMalformed {
        /// Transaction decoder error.
        #[source]
        source: std::io::Error,
    },
    /// Bytes follow the canonical transaction. Retry posture: `not_retryable`.
    #[error("disclosed transaction has {trailing_bytes} trailing bytes")]
    TransactionTrailingBytes {
        /// Count of unconsumed bytes.
        trailing_bytes: usize,
    },
    /// The fetched transaction does not match the disclosure. Retry posture: `requires_operator`.
    #[error(
        "payment disclosure references transaction {expected_transaction_id}, but the fetched transaction is {actual_transaction_id}"
    )]
    TransactionIdMismatch {
        /// Transaction identifier committed to by the disclosure.
        expected_transaction_id: TxId,
        /// Identifier computed from the supplied transaction bytes.
        actual_transaction_id: TxId,
    },
    /// The referenced transaction has no Sapling bundle. Retry posture: `not_retryable`.
    #[error("disclosed transaction has no Sapling bundle")]
    SaplingBundleMissing,
    /// The referenced transaction has no Ironwood bundle. Retry posture: `not_retryable`.
    #[error("disclosed transaction has no Ironwood bundle")]
    IronwoodBundleMissing,
    /// A selected Ironwood action index is absent. Retry posture: `not_retryable`.
    #[error("Ironwood action index {index} is absent from a bundle with {action_count} actions")]
    IronwoodActionIndexOutOfBounds {
        /// Selected action index.
        index: u32,
        /// Number of on-chain Ironwood actions.
        action_count: usize,
    },
    /// The message-bound signature is invalid for the mined action key. Retry posture: `not_retryable`.
    #[error("Ironwood action {index} does not authorize the disclosure message")]
    IronwoodSpendAuthorityInvalid {
        /// Selected action index.
        index: u32,
    },
    /// The OCK does not recover the selected Ironwood output. Retry posture: `not_retryable`.
    #[error(
        "Ironwood action {index} output cannot be recovered with the disclosed outgoing cipher key"
    )]
    IronwoodOutputRecoveryFailed {
        /// Selected action index.
        index: u32,
    },
    /// A selected Sapling spend index is absent. Retry posture: `not_retryable`.
    #[error("Sapling spend index {index} is absent from a bundle with {spend_count} spends")]
    SpendIndexOutOfBounds {
        /// Selected spend index.
        index: u32,
        /// Number of on-chain Sapling spends.
        spend_count: usize,
    },
    /// A selected Sapling output index is absent. Retry posture: `not_retryable`.
    #[error("Sapling output index {index} is absent from a bundle with {output_count} outputs")]
    OutputIndexOutOfBounds {
        /// Selected output index.
        index: u32,
        /// Number of on-chain Sapling outputs.
        output_count: usize,
    },
    /// The disclosed Sapling spend commitment is malformed. Retry posture: `not_retryable`.
    #[error("Sapling spend {index} has a malformed commitment")]
    SpendCommitmentMalformed {
        /// Selected spend index.
        index: u32,
    },
    /// The disclosed randomized verification key is malformed. Retry posture: `not_retryable`.
    #[error("Sapling spend {index} has a malformed randomized verification key: {source}")]
    RandomizedVerificationKeyMalformed {
        /// Selected spend index.
        index: u32,
        /// `RedJubjub` decoder error.
        #[source]
        source: redjubjub::Error,
    },
    /// The disclosed Groth16 proof is malformed. Retry posture: `not_retryable`.
    #[error("Sapling spend {index} has a malformed proof: {source}")]
    SpendProofMalformed {
        /// Selected spend index.
        index: u32,
        /// Groth16 decoder error.
        #[source]
        source: std::io::Error,
    },
    /// The proof or spend authorization signature is invalid. Retry posture: `not_retryable`.
    #[error("Sapling spend {index} does not prove authority over the on-chain nullifier")]
    SpendAuthorityInvalid {
        /// Selected spend index.
        index: u32,
    },
    /// The OCK does not recover and authenticate the selected output. Retry posture: `not_retryable`.
    #[error("Sapling output {index} cannot be recovered with the disclosed outgoing cipher key")]
    OutputRecoveryFailed {
        /// Selected output index.
        index: u32,
    },
}
