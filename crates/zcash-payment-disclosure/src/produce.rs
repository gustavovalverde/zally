use std::fmt;

use ff::Field;
use orchard::{
    keys::SpendAuthorizingKey as IronwoodSpendAuthorizingKey,
    primitives::redpallas::{
        SigningKey as IronwoodSigningKey, SpendAuth as IronwoodSpendAuth,
        VerificationKey as IronwoodVerificationKey,
    },
};
use pasta_curves::pallas;
use rand_core::{CryptoRng, RngCore};
use redjubjub::{SigningKey, SpendAuth, VerificationKey};
use sapling::{
    Anchor, MerklePath, PaymentAddress, ProofGenerationKey, Rseed,
    keys::{SpendAuthorizingKey, SpendValidatingKey},
    prover::SpendProver,
    value::{NoteValue, ValueCommitTrapdoor, ValueCommitment},
};
use zcash_protocol::{TxId, consensus::NetworkType};

use crate::{
    IronwoodOutputDisclosure, IronwoodSpendDisclosure, PaymentDisclosure,
    PaymentDisclosureCodecError, PaymentDisclosureProfile, SaplingOutputDisclosure,
    SaplingSpendDisclosure,
    codec::{validate_disclosure_shape, validate_ironwood_disclosure_shape},
};

/// Complete, owned inputs for recreating one Sapling spend-authority proof.
#[derive(Clone)]
pub struct SaplingSpendProvingInput {
    index: u32,
    recipient: PaymentAddress,
    amount_zat: u64,
    rseed: Rseed,
    proof_generation_key: ProofGenerationKey,
    anchor: Anchor,
    witness: MerklePath,
}

impl fmt::Debug for SaplingSpendProvingInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SaplingSpendProvingInput")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl SaplingSpendProvingInput {
    /// Constructs the proving inputs for one on-chain Sapling spend index.
    #[allow(
        clippy::too_many_arguments,
        reason = "all Sapling Spend circuit witnesses are required"
    )]
    #[must_use]
    pub const fn new(
        index: u32,
        recipient: PaymentAddress,
        amount_zat: u64,
        rseed: Rseed,
        proof_generation_key: ProofGenerationKey,
        anchor: Anchor,
        witness: MerklePath,
    ) -> Self {
        Self {
            index,
            recipient,
            amount_zat,
            rseed,
            proof_generation_key,
            anchor,
            witness,
        }
    }

    /// Returns the selected Sapling spend index.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }
}

/// One Sapling output selected for disclosure by its outgoing cipher key.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct SaplingOutputSelection {
    index: u32,
    ock: [u8; 32],
}

impl fmt::Debug for SaplingOutputSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SaplingOutputSelection")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl SaplingOutputSelection {
    /// Constructs a selected Sapling output.
    #[must_use]
    pub const fn new(index: u32, ock: [u8; 32]) -> Self {
        Self { index, ock }
    }

    /// Returns the selected Sapling output index.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }
}

/// Retained signing material for one mined Ironwood spend action.
#[derive(Clone)]
pub struct IronwoodSpendSigningInput {
    index: u32,
    alpha: pallas::Scalar,
    randomized_verification_key: IronwoodVerificationKey<IronwoodSpendAuth>,
}

impl fmt::Debug for IronwoodSpendSigningInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IronwoodSpendSigningInput")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl IronwoodSpendSigningInput {
    /// Constructs retained Ironwood spend signing material.
    #[must_use]
    pub const fn new(
        index: u32,
        alpha: pallas::Scalar,
        randomized_verification_key: IronwoodVerificationKey<IronwoodSpendAuth>,
    ) -> Self {
        Self {
            index,
            alpha,
            randomized_verification_key,
        }
    }

    /// Returns the selected Ironwood action index.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }
}

/// One Ironwood output selected for disclosure by its outgoing cipher key.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct IronwoodOutputSelection {
    index: u32,
    ock: [u8; 32],
}

impl fmt::Debug for IronwoodOutputSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IronwoodOutputSelection")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl IronwoodOutputSelection {
    /// Constructs a selected Ironwood output.
    #[must_use]
    pub const fn new(index: u32, ock: [u8; 32]) -> Self {
        Self { index, ock }
    }

    /// Returns the selected Ironwood action index.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }
}

/// A validated plan for producing one Zally Ironwood extension disclosure.
pub struct IronwoodDisclosurePlan {
    transaction_id: TxId,
    network: NetworkType,
    message: Vec<u8>,
    ironwood_spends: Vec<IronwoodSpendSigningInput>,
    ironwood_outputs: Vec<IronwoodOutputSelection>,
}

impl IronwoodDisclosurePlan {
    /// Constructs and validates an Ironwood extension production plan.
    ///
    /// # Errors
    ///
    /// Returns an error when no spend is selected, the message exceeds the profile bound,
    /// or selected indices are not strictly increasing.
    pub fn new(
        transaction_id: TxId,
        network: NetworkType,
        message: Vec<u8>,
        ironwood_spends: Vec<IronwoodSpendSigningInput>,
        ironwood_outputs: Vec<IronwoodOutputSelection>,
    ) -> Result<Self, PaymentDisclosureCodecError> {
        validate_ironwood_disclosure_shape(
            message.len(),
            ironwood_spends.len(),
            ironwood_spends.iter().map(IronwoodSpendSigningInput::index),
            ironwood_outputs.len(),
            ironwood_outputs.iter().map(IronwoodOutputSelection::index),
        )?;
        Ok(Self {
            transaction_id,
            network,
            message,
            ironwood_spends,
            ironwood_outputs,
        })
    }
}

/// A validated plan for producing one immutable Draft1 payment disclosure.
pub struct PaymentDisclosurePlan {
    transaction_id: TxId,
    network: NetworkType,
    message: Vec<u8>,
    sapling_spends: Vec<SaplingSpendProvingInput>,
    sapling_outputs: Vec<SaplingOutputSelection>,
}

impl fmt::Debug for PaymentDisclosurePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaymentDisclosurePlan")
            .field("transaction_id", &self.transaction_id)
            .field("network", &self.network)
            .field("message_bytes", &self.message.len())
            .field("sapling_spends", &self.sapling_spends.len())
            .field("sapling_outputs", &self.sapling_outputs.len())
            .finish()
    }
}

impl PaymentDisclosurePlan {
    /// Constructs and validates a Draft1 production plan before any proving work begins.
    ///
    /// # Errors
    ///
    /// Returns an error when no spend is selected, the message exceeds the profile bound,
    /// or selected indices are not strictly increasing.
    pub fn new(
        transaction_id: TxId,
        network: NetworkType,
        message: Vec<u8>,
        sapling_spends: Vec<SaplingSpendProvingInput>,
        sapling_outputs: Vec<SaplingOutputSelection>,
    ) -> Result<Self, PaymentDisclosureCodecError> {
        validate_disclosure_shape(
            message.len(),
            sapling_spends.len(),
            sapling_spends.iter().map(SaplingSpendProvingInput::index),
            sapling_outputs.len(),
            sapling_outputs.iter().map(SaplingOutputSelection::index),
        )?;
        Ok(Self {
            transaction_id,
            network,
            message,
            sapling_spends,
            sapling_outputs,
        })
    }
}

struct UnsignedSaplingSpend {
    index: u32,
    cv: [u8; 32],
    rk: VerificationKey<SpendAuth>,
    zkproof: [u8; 192],
    randomized_ask: SigningKey<SpendAuth>,
}

struct UnsignedIronwoodSpend {
    index: u32,
    randomized_signing_key: IronwoodSigningKey<IronwoodSpendAuth>,
    randomized_verification_key: IronwoodVerificationKey<IronwoodSpendAuth>,
}

/// Produces a Draft1 payment disclosure with fresh Sapling proof randomness.
///
/// The caller owns the authorizing key, Spend prover, and cryptographically secure RNG.
/// None of them are retained in the returned disclosure.
///
/// # Errors
///
/// Returns an error when the authorizing key does not match a spend, a Spend circuit cannot
/// be prepared, or an internally-created randomized key or signature does not verify.
pub fn prove_disclosure<SpendProverT, RngT>(
    plan: PaymentDisclosurePlan,
    ask: &SpendAuthorizingKey,
    spend_prover: &SpendProverT,
    rng: &mut RngT,
) -> Result<PaymentDisclosure, PaymentDisclosureProductionError>
where
    SpendProverT: SpendProver,
    RngT: RngCore + CryptoRng,
{
    let PaymentDisclosurePlan {
        transaction_id,
        network,
        message,
        sapling_spends,
        sapling_outputs,
    } = plan;
    let expected_ak = SpendValidatingKey::from(ask);
    let mut unsigned_spends = Vec::with_capacity(sapling_spends.len());
    for spend in sapling_spends {
        if spend.proof_generation_key.ak != expected_ak {
            return Err(
                PaymentDisclosureProductionError::SpendAuthorizingKeyMismatch {
                    index: spend.index,
                },
            );
        }
        unsigned_spends.push(prove_sapling_spend(spend, ask, spend_prover, rng)?);
    }
    let disclosed_outputs: Vec<_> = sapling_outputs
        .into_iter()
        .map(|output| SaplingOutputDisclosure::new(output.index, output.ock))
        .collect();
    let unsigned_disclosure = PaymentDisclosure::new(
        PaymentDisclosureProfile::Zip311Draft1,
        transaction_id,
        message.clone(),
        unsigned_spends
            .iter()
            .map(|spend| {
                SaplingSpendDisclosure::new(
                    spend.index,
                    spend.cv,
                    spend.rk.into(),
                    spend.zkproof,
                    [0; 64],
                )
            })
            .collect(),
        disclosed_outputs.clone(),
    )?;
    let disclosure_digest = unsigned_disclosure.compute_digest(network);

    let mut signed_spends = Vec::with_capacity(unsigned_spends.len());
    for spend in unsigned_spends {
        let spend_auth_sig = spend.randomized_ask.sign(&mut *rng, &disclosure_digest);
        if spend
            .rk
            .verify(&disclosure_digest, &spend_auth_sig)
            .is_err()
        {
            return Err(PaymentDisclosureProductionError::SpendSignatureInvalid {
                index: spend.index,
            });
        }
        signed_spends.push(SaplingSpendDisclosure::new(
            spend.index,
            spend.cv,
            spend.rk.into(),
            spend.zkproof,
            spend_auth_sig.into(),
        ));
    }

    PaymentDisclosure::new(
        PaymentDisclosureProfile::Zip311Draft1,
        transaction_id,
        message,
        signed_spends,
        disclosed_outputs,
    )
    .map_err(Into::into)
}

/// Produces a Zally Ironwood extension disclosure bound to mined action keys.
///
/// Unlike Draft1 Sapling production, this profile relies on the mined Ironwood bundle's
/// consensus-verified proof to bind each action's randomized verification key to its nullifier.
/// The retained PCZT randomizer authorizes the same key over the disclosure digest.
///
/// # Errors
///
/// Returns an error when the authorizing key and retained randomizer do not reproduce an
/// action's randomized verification key, or a created signature fails self-verification.
pub fn sign_ironwood_disclosure<RngT>(
    plan: IronwoodDisclosurePlan,
    ask: &IronwoodSpendAuthorizingKey,
    rng: &mut RngT,
) -> Result<PaymentDisclosure, PaymentDisclosureProductionError>
where
    RngT: RngCore + CryptoRng,
{
    let IronwoodDisclosurePlan {
        transaction_id,
        network,
        message,
        ironwood_spends,
        ironwood_outputs,
    } = plan;
    let mut unsigned_spends = Vec::with_capacity(ironwood_spends.len());
    for spend in ironwood_spends {
        let randomized_signing_key = ask.randomize(&spend.alpha);
        let randomized_verification_key = IronwoodVerificationKey::from(&randomized_signing_key);
        if randomized_verification_key != spend.randomized_verification_key {
            return Err(
                PaymentDisclosureProductionError::IronwoodSpendAuthorizingKeyMismatch {
                    index: spend.index,
                },
            );
        }
        unsigned_spends.push(UnsignedIronwoodSpend {
            index: spend.index,
            randomized_signing_key,
            randomized_verification_key,
        });
    }
    let disclosed_outputs: Vec<_> = ironwood_outputs
        .into_iter()
        .map(|output| IronwoodOutputDisclosure::new(output.index, output.ock))
        .collect();
    let unsigned_disclosure = PaymentDisclosure::ironwood_extension(
        transaction_id,
        message.clone(),
        unsigned_spends
            .iter()
            .map(|spend| IronwoodSpendDisclosure::new(spend.index, [0; 64]))
            .collect(),
        disclosed_outputs.clone(),
    )?;
    let disclosure_digest = unsigned_disclosure.compute_digest(network);
    let mut signed_spends = Vec::with_capacity(unsigned_spends.len());
    for spend in unsigned_spends {
        let spend_auth_sig = spend
            .randomized_signing_key
            .sign(&mut *rng, &disclosure_digest);
        if spend
            .randomized_verification_key
            .verify(&disclosure_digest, &spend_auth_sig)
            .is_err()
        {
            return Err(
                PaymentDisclosureProductionError::IronwoodSpendSignatureInvalid {
                    index: spend.index,
                },
            );
        }
        signed_spends.push(IronwoodSpendDisclosure::new(
            spend.index,
            <[u8; 64]>::from(&spend_auth_sig),
        ));
    }
    PaymentDisclosure::ironwood_extension(transaction_id, message, signed_spends, disclosed_outputs)
        .map_err(Into::into)
}

fn prove_sapling_spend<SpendProverT, RngT>(
    spend: SaplingSpendProvingInput,
    ask: &SpendAuthorizingKey,
    spend_prover: &SpendProverT,
    rng: &mut RngT,
) -> Result<UnsignedSaplingSpend, PaymentDisclosureProductionError>
where
    SpendProverT: SpendProver,
    RngT: RngCore + CryptoRng,
{
    let alpha = jubjub::Fr::random(&mut *rng);
    let rcv = ValueCommitTrapdoor::random(&mut *rng);
    let note_amount = NoteValue::from_raw(spend.amount_zat);
    let cv = ValueCommitment::derive(note_amount, rcv.clone());
    let rk = spend.proof_generation_key.ak.randomize(&alpha);
    let randomized_ask = ask.randomize(&alpha);
    if VerificationKey::from(&randomized_ask) != rk {
        return Err(
            PaymentDisclosureProductionError::RandomizedVerificationKeyMismatch {
                index: spend.index,
            },
        );
    }
    let anchor = Option::from(bls12_381::Scalar::from_bytes(&spend.anchor.to_bytes()))
        .ok_or(PaymentDisclosureProductionError::SpendCircuitInvalid { index: spend.index })?;
    let circuit = SpendProverT::prepare_circuit(
        spend.proof_generation_key,
        *spend.recipient.diversifier(),
        spend.rseed,
        note_amount,
        alpha,
        rcv,
        anchor,
        spend.witness,
    )
    .ok_or(PaymentDisclosureProductionError::SpendCircuitInvalid { index: spend.index })?;
    let proof = spend_prover.create_proof(circuit, &mut *rng);
    Ok(UnsignedSaplingSpend {
        index: spend.index,
        cv: cv.to_bytes(),
        rk,
        zkproof: SpendProverT::encode_proof(proof),
        randomized_ask,
    })
}

/// Failure to produce a supported payment-disclosure profile.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PaymentDisclosureProductionError {
    /// The authorizing key does not own a selected Ironwood spend. Retry posture: `not_retryable`.
    #[error("spend authorizing key does not match Ironwood action {index}")]
    IronwoodSpendAuthorizingKeyMismatch {
        /// Selected action index.
        index: u32,
    },
    /// A new Ironwood authorization signature did not verify. Retry posture: `requires_operator`.
    #[error("Ironwood action {index} produced an invalid authorization signature")]
    IronwoodSpendSignatureInvalid {
        /// Selected action index.
        index: u32,
    },
    /// The authorizing key does not own a selected spend. Retry posture: `not_retryable`.
    #[error("spend authorizing key does not match Sapling spend {index}")]
    SpendAuthorizingKeyMismatch {
        /// Selected spend index.
        index: u32,
    },
    /// The Sapling Spend circuit could not be prepared. Retry posture: `not_retryable`.
    #[error("Sapling spend {index} has an invalid proving circuit")]
    SpendCircuitInvalid {
        /// Selected spend index.
        index: u32,
    },
    /// Fresh secret and public randomization disagreed. Retry posture: `requires_operator`.
    #[error("Sapling spend {index} produced inconsistent randomized verification keys")]
    RandomizedVerificationKeyMismatch {
        /// Selected spend index.
        index: u32,
    },
    /// A newly-created authorization signature did not verify. Retry posture: `requires_operator`.
    #[error("Sapling spend {index} produced an invalid authorization signature")]
    SpendSignatureInvalid {
        /// Selected spend index.
        index: u32,
    },
    /// Final disclosure construction violated the selected profile's codec. Retry posture: `not_retryable`.
    #[error(transparent)]
    Codec(#[from] PaymentDisclosureCodecError),
}

#[cfg(test)]
mod tests {
    use incrementalmerkletree::Position;
    use rand::{SeedableRng, rngs::StdRng};
    use redjubjub::{Signature, SpendAuth, VerificationKey};
    use sapling::{
        MerklePath, Node, Rseed, prover::mock::MockSpendProver, zip32::ExtendedSpendingKey,
    };

    use super::*;

    #[test]
    fn spend_signature_binds_the_message_and_network() -> Result<(), Box<dyn std::error::Error>> {
        let spending_key = ExtendedSpendingKey::master(&[7; 32]);
        let (_, recipient) = spending_key.default_address();
        let node = Option::from(Node::from_bytes([0; 32])).ok_or("invalid fixture node")?;
        let witness = MerklePath::from_parts(vec![node; 32], Position::from(0))
            .map_err(|()| "invalid fixture witness")?;
        let plan = PaymentDisclosurePlan::new(
            TxId::from_bytes([0x42; 32]),
            NetworkType::Test,
            b"challenge-a".to_vec(),
            vec![SaplingSpendProvingInput::new(
                0,
                recipient,
                50_000,
                Rseed::AfterZip212([9; 32]),
                spending_key.expsk.proof_generation_key(),
                sapling::Anchor::from(sapling::Node::from_scalar(bls12_381::Scalar::from(11))),
                witness,
            )],
            Vec::new(),
        )?;
        let disclosure = prove_disclosure(
            plan,
            &spending_key.expsk.ask,
            &MockSpendProver,
            &mut StdRng::from_seed([3; 32]),
        )?;
        let spend = &disclosure.sapling_spends()[0];
        let rk = VerificationKey::<SpendAuth>::try_from(spend.rk_bytes())?;
        let signature = Signature::<SpendAuth>::from(spend.spend_auth_sig_bytes());
        assert!(
            rk.verify(&disclosure.compute_digest(NetworkType::Test), &signature,)
                .is_ok()
        );

        let changed_message = PaymentDisclosure::new(
            disclosure.profile(),
            disclosure.transaction_id(),
            b"challenge-b".to_vec(),
            vec![spend.clone()],
            disclosure.sapling_outputs().to_vec(),
        )?;
        assert!(
            rk.verify(
                &changed_message.compute_digest(NetworkType::Test),
                &signature,
            )
            .is_err()
        );
        assert!(
            rk.verify(&disclosure.compute_digest(NetworkType::Main), &signature,)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn ironwood_signature_binds_the_message_and_mined_action_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let spending_key = Option::from(orchard::keys::SpendingKey::from_bytes([7; 32]))
            .ok_or("invalid Orchard spending key")?;
        let ask = orchard::keys::SpendAuthorizingKey::from(&spending_key);
        let alpha = pallas::Scalar::from(9_u64);
        let randomized_signing_key = ask.randomize(&alpha);
        let randomized_verification_key =
            IronwoodVerificationKey::<IronwoodSpendAuth>::from(&randomized_signing_key);
        let plan = IronwoodDisclosurePlan::new(
            TxId::from_bytes([0x24; 32]),
            NetworkType::Test,
            b"challenge-a".to_vec(),
            vec![IronwoodSpendSigningInput::new(
                0,
                alpha,
                randomized_verification_key.clone(),
            )],
            Vec::new(),
        )?;
        let disclosure = sign_ironwood_disclosure(plan, &ask, &mut StdRng::from_seed([5; 32]))?;
        let signature = orchard::primitives::redpallas::Signature::<IronwoodSpendAuth>::from(
            disclosure.ironwood_spends()[0].spend_auth_sig_bytes(),
        );

        assert!(
            randomized_verification_key
                .verify(&disclosure.compute_digest(NetworkType::Test), &signature)
                .is_ok()
        );
        let changed_message = PaymentDisclosure::ironwood_extension(
            disclosure.transaction_id(),
            b"challenge-b".to_vec(),
            disclosure.ironwood_spends().to_vec(),
            disclosure.ironwood_outputs().to_vec(),
        )?;
        assert!(
            randomized_verification_key
                .verify(
                    &changed_message.compute_digest(NetworkType::Test),
                    &signature,
                )
                .is_err()
        );
        Ok(())
    }
}
