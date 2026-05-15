//! `Prover` role: creates Sapling and Orchard proofs for a PCZT.
//!
//! Derives the account-zero ZIP-32 `UnifiedSpendingKey` from the supplied seed so Sapling
//! proof-generation keys can be inserted before proving. Orchard proof creation does not
//! require the spending key, but the same single-call surface keeps the in-process Zally
//! wallet flow ergonomic.

use std::sync::OnceLock;

use pczt::roles::{prover::Prover as UpstreamProver, updater::Updater};
use zally_core::Network;
use zally_keys::SeedMaterial;
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_proofs::prover::LocalTxProver;

use crate::pczt_bytes::PcztBytes;
use crate::pczt_error::PcztError;

static ORCHARD_PROVING_KEY: OnceLock<orchard::circuit::ProvingKey> = OnceLock::new();

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

    /// Creates required Sapling and Orchard proofs using keys derived from `seed`.
    ///
    /// The Sapling proving path needs both Sapling proving parameters and the account's
    /// proof-generation key. Orchard proving uses a process-local proving-key cache.
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
        let mut prover = UpstreamProver::new(pczt);
        if prover.requires_orchard_proof() {
            prover = prover
                .create_orchard_proof(orchard_proving_key())
                .map_err(|err| PcztError::UpstreamFailed {
                    reason: format!("orchard proof creation failed: {err:?}"),
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

        Ok(PcztBytes::from_pczt(&prover.finish(), self.network))
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

fn orchard_proving_key() -> &'static orchard::circuit::ProvingKey {
    ORCHARD_PROVING_KEY.get_or_init(orchard::circuit::ProvingKey::build)
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
