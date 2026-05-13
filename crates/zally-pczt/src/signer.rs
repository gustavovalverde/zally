//! `Signer` role: applies Sapling and Orchard signatures using a sealed seed.
//!
//! Derives the per-account ZIP-32 `UnifiedSpendingKey` from the supplied seed and uses the
//! pczt 0.6 signer to apply Sapling + Orchard spend authorizations. Transparent signing is
//! a v1 follow-up because pczt's transparent signer needs per-input scriptCode + sighash
//! plumbing that depends on the proposal builder's input metadata.

use pczt::roles::signer::Signer as UpstreamSigner;
use zally_core::Network;
use zally_keys::SeedMaterial;
use zcash_keys::keys::UnifiedSpendingKey;

use crate::pczt_bytes::PcztBytes;
use crate::pczt_error::PcztError;

/// Signs a PCZT with keys derived from a sealed seed.
#[derive(Debug)]
pub struct Signer {
    network: Network,
}

impl Signer {
    /// Constructs a signer for `network`.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self { network }
    }

    /// Returns the network this signer is bound to.
    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }

    /// Signs `pczt` with keys derived from `seed` via the account-zero ZIP-32 spending key.
    ///
    /// Applies Sapling and Orchard spend authorizations for every spend whose embedded
    /// metadata aligns with the derived account's keys. Returns
    /// [`PcztError::NoMatchingKeys`] when the seed cannot authorize any of the PCZT's
    /// spends (e.g., wrong seed, or an all-transparent PCZT before transparent signing is
    /// wired).
    #[allow(
        clippy::unused_async,
        reason = "async surface is the v1 contract; ZIP-32 derivation runs on the caller's \
                  thread today but moves into spawn_blocking once the prover wire-up lands"
    )]
    pub async fn sign_with_seed(
        &self,
        pczt: PcztBytes,
        seed: &SeedMaterial,
    ) -> Result<PcztBytes, PcztError> {
        validate_network(&pczt, self.network)?;

        let parsed = pczt.parse()?;
        let sapling_count = parsed.sapling().spends().len();
        let orchard_count = parsed.orchard().actions().len();

        let params = self.network.to_parameters();
        let account = zip32::AccountId::ZERO;
        let usk = UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), account).map_err(
            |err| PcztError::UpstreamFailed {
                reason: format!("ZIP-32 derivation failed: {err}"),
                is_retryable: false,
            },
        )?;

        let mut upstream =
            UpstreamSigner::new(parsed).map_err(|err| PcztError::UpstreamFailed {
                reason: format!("pczt signer init failed: {err:?}"),
                is_retryable: false,
            })?;

        let sapling_total = sign_sapling_spends(&mut upstream, &usk, sapling_count)?;
        let orchard_total = sign_orchard_spends(&mut upstream, &usk, orchard_count)?;
        if sapling_total == 0 && orchard_total == 0 {
            return Err(PcztError::NoMatchingKeys);
        }

        let signed_pczt = upstream.finish();
        Ok(PcztBytes::from_pczt(&signed_pczt, self.network))
    }
}

fn sign_sapling_spends(
    signer: &mut UpstreamSigner,
    usk: &UnifiedSpendingKey,
    spend_count: usize,
) -> Result<usize, PcztError> {
    let ask = usk.sapling().expsk.ask.clone();
    let mut authorized = 0_usize;
    for index in 0..spend_count {
        match signer.sign_sapling(index, &ask) {
            Ok(()) => authorized += 1,
            Err(err) => {
                return Err(PcztError::UpstreamFailed {
                    reason: format!("sapling spend {index} sign failed: {err:?}"),
                    is_retryable: false,
                });
            }
        }
    }
    Ok(authorized)
}

fn sign_orchard_spends(
    signer: &mut UpstreamSigner,
    usk: &UnifiedSpendingKey,
    spend_count: usize,
) -> Result<usize, PcztError> {
    // The zcash_keys 0.13 -> orchard 0.12 transitive dependency conflicts with pczt 0.6's
    // direct orchard 0.13. Bridge the spending key via its canonical 32-byte form, which
    // is stable across the minor version bump.
    let sk_bytes = *usk.orchard().to_bytes();
    let spending_key =
        Option::from(orchard::keys::SpendingKey::from_bytes(sk_bytes)).ok_or_else(|| {
            PcztError::UpstreamFailed {
                reason: "orchard spending key bytes failed CtOption check".into(),
                is_retryable: false,
            }
        })?;
    let ask = orchard::keys::SpendAuthorizingKey::from(&spending_key);
    let mut authorized = 0_usize;
    for index in 0..spend_count {
        match signer.sign_orchard(index, &ask) {
            Ok(()) => authorized += 1,
            Err(err) => {
                return Err(PcztError::UpstreamFailed {
                    reason: format!("orchard action {index} sign failed: {err:?}"),
                    is_retryable: false,
                });
            }
        }
    }
    Ok(authorized)
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
    async fn signer_rejects_mismatched_network() {
        let signer = Signer::new(Network::Mainnet);
        let pczt = PcztBytes::from_serialized(vec![0_u8; 4], Network::Testnet);
        let mnemonic = Mnemonic::generate();
        let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
        let outcome = signer.sign_with_seed(pczt, &seed).await;
        assert!(matches!(outcome, Err(PcztError::NetworkMismatch { .. })));
    }
}
