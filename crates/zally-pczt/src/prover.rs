//! `Prover` role: creates Sapling, Orchard, and Ironwood proofs for a PCZT.
//!
//! Derives the account-zero ZIP-32 `UnifiedSpendingKey` from the supplied seed so Sapling
//! proof-generation keys can be inserted before proving. Orchard and Ironwood proof creation
//! do not require the spending key, but the same single-call surface keeps the in-process
//! Zally wallet flow ergonomic.

use std::sync::OnceLock;

use pczt::roles::{prover::Prover as UpstreamProver, updater::Updater};
use zally_core::Network;
use zally_keys::SeedMaterial;
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_proofs::prover::LocalTxProver;

use crate::bytes::PcztBytes;
use crate::error::PcztError;

static ORCHARD_PROVING_KEY_FIXED_POST_NU6_2: OnceLock<orchard::circuit::ProvingKey> =
    OnceLock::new();
static ORCHARD_PROVING_KEY_POST_NU6_3: OnceLock<orchard::circuit::ProvingKey> = OnceLock::new();
static IRONWOOD_PROVING_KEY: OnceLock<orchard::circuit::ProvingKey> = OnceLock::new();

/// Creates zero-knowledge proofs for a PCZT.
#[derive(Debug)]
pub struct Prover {
    network: Network,
}

impl Prover {
    /// Constructs a prover for `network`.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self { network }
    }

    /// Returns the network this prover is bound to.
    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }

    /// Creates required Sapling, Orchard, and Ironwood proofs using keys derived from `seed`.
    ///
    /// The Sapling proving path needs both Sapling proving parameters and the account's
    /// proof-generation key. Orchard and Ironwood proving use process-local proving-key caches.
    ///
    /// `requires_operator` when Sapling parameters are unavailable; `not_retryable` when
    /// the PCZT is malformed or upstream proof creation rejects its contents.
    #[allow(
        clippy::unused_async,
        reason = "async surface matches the other zally-pczt role APIs and leaves room for scheduled proof creation"
    )]
    pub async fn prove_with_seed(
        &self,
        pczt: PcztBytes,
        seed: &SeedMaterial,
    ) -> Result<PcztBytes, PcztError> {
        validate_network(&pczt, self.network)?;

        let params = self.network.to_parameters();
        let usk =
            UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), zip32::AccountId::ZERO)
                .map_err(|err| PcztError::UpstreamFailed {
                    reason: format!("ZIP-32 derivation failed: {err}"),
                    is_retryable: false,
                })?;

        let pczt = add_sapling_proof_generation_keys(pczt.parse()?, &usk)?;
        let consensus_branch_id = *pczt.global().consensus_branch_id();
        let mut prover = UpstreamProver::new(pczt);
        if prover.requires_orchard_proof() {
            prover = prover
                .create_orchard_proof(orchard_proving_key(consensus_branch_id)?)
                .map_err(|err| PcztError::UpstreamFailed {
                    reason: format!("orchard proof creation failed: {err:?}"),
                    is_retryable: false,
                })?;
        }
        if prover.requires_ironwood_proof() {
            prover = prover
                .create_ironwood_proof(ironwood_proving_key())
                .map_err(|err| PcztError::UpstreamFailed {
                    reason: format!("ironwood proof creation failed: {err:?}"),
                    is_retryable: false,
                })?;
        }
        if prover.requires_sapling_proofs() {
            let sapling_prover =
                LocalTxProver::with_default_location().ok_or(PcztError::ProverUnavailable)?;
            prover = prover
                .create_sapling_proofs(&sapling_prover, &sapling_prover)
                .map_err(|err| PcztError::UpstreamFailed {
                    reason: format!("sapling proof creation failed: {err:?}"),
                    is_retryable: false,
                })?;
        }

        PcztBytes::from_pczt(prover.finish(), self.network)
    }
}

fn add_sapling_proof_generation_keys(
    pczt: pczt::Pczt,
    usk: &UnifiedSpendingKey,
) -> Result<pczt::Pczt, PcztError> {
    Updater::new(pczt)
        .update_sapling_with(|mut updater| {
            let spend_indices = updater
                .bundle()
                .spends()
                .iter()
                .enumerate()
                .filter_map(|(index, spend)| {
                    spend.proof_generation_key().is_none().then_some(index)
                })
                .collect::<Vec<_>>();

            for index in spend_indices {
                updater.update_spend_with(index, |mut spend_updater| {
                    spend_updater
                        .set_proof_generation_key(usk.sapling().expsk.proof_generation_key())
                })?;
            }

            Ok(())
        })
        .map_err(|err| PcztError::UpstreamFailed {
            reason: format!("sapling proof-generation key update failed: {err:?}"),
            is_retryable: false,
        })
        .map(Updater::finish)
}

/// Builds (and caches) the Orchard proving key for the circuit version `consensus_branch_id`
/// requires.
///
/// Orchard bundles built under different network upgrades require different, incompatible
/// circuits: the fixed post-NU6.2 circuit and the post-NU6.3 circuit produce distinct
/// proving/verifying keys, so a proof made with the wrong one is rejected by the upstream
/// prover outright. Refuses to prove under the insecure pre-NU6.2 circuit, which upstream
/// reserves for reconstructing historical verifying keys, never for creating new proofs.
fn orchard_proving_key(
    consensus_branch_id: u32,
) -> Result<&'static orchard::circuit::ProvingKey, PcztError> {
    use zcash_protocol::consensus::OrchardProtocolRevision;

    let branch_id =
        zcash_protocol::consensus::BranchId::try_from(consensus_branch_id).map_err(|reason| {
            PcztError::UpstreamFailed {
                reason: format!("unrecognized consensus branch id: {reason}"),
                is_retryable: false,
            }
        })?;
    let revision =
        branch_id
            .orchard_protocol_revision()
            .ok_or_else(|| PcztError::UpstreamFailed {
                reason: format!("consensus branch {branch_id:?} predates the Orchard protocol"),
                is_retryable: false,
            })?;

    match revision {
        OrchardProtocolRevision::InsecureV1 => Err(PcztError::UpstreamFailed {
            reason: "refusing to create an Orchard proof under the insecure pre-NU6.2 circuit"
                .into(),
            is_retryable: false,
        }),
        OrchardProtocolRevision::V2 => Ok(ORCHARD_PROVING_KEY_FIXED_POST_NU6_2.get_or_init(|| {
            orchard::circuit::ProvingKey::build(
                orchard::circuit::OrchardCircuitVersion::FixedPostNu6_2,
            )
        })),
        OrchardProtocolRevision::V3 => Ok(ORCHARD_PROVING_KEY_POST_NU6_3.get_or_init(|| {
            orchard::circuit::ProvingKey::build(orchard::circuit::OrchardCircuitVersion::PostNu6_3)
        })),
    }
}

/// Builds (and caches) the Ironwood proving key for the post-NU6.3 circuit.
fn ironwood_proving_key() -> &'static orchard::circuit::ProvingKey {
    IRONWOOD_PROVING_KEY.get_or_init(|| {
        orchard::circuit::ProvingKey::build(orchard::circuit::OrchardCircuitVersion::PostNu6_3)
    })
}

fn validate_network(pczt: &PcztBytes, configured: Network) -> Result<(), PcztError> {
    if pczt.network() == configured {
        Ok(())
    } else {
        Err(PcztError::mismatch(pczt.network(), configured))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zally_keys::Mnemonic;

    #[tokio::test]
    async fn prover_rejects_mismatched_network() {
        let prover = Prover::new(Network::Mainnet);
        let pczt = PcztBytes::from_serialized(vec![0_u8; 4], Network::Testnet);
        let mnemonic = Mnemonic::generate();
        let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
        let outcome = prover.prove_with_seed(pczt, &seed).await;
        assert!(matches!(outcome, Err(PcztError::NetworkMismatch { .. })));
    }
}
