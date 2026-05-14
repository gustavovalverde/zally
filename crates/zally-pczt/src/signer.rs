//! `Signer` role: applies Sapling, Orchard, and transparent signatures using a sealed seed.
//!
//! Derives the per-account ZIP-32 `UnifiedSpendingKey` from the supplied seed and uses the
//! pczt 0.6 signer to apply each pool's spend authorization. Transparent inputs are matched
//! against the wallet's external- and internal-scope addresses within the standard BIP-44
//! gap limit; matched inputs are signed by deriving the corresponding `secp256k1::SecretKey`
//! and calling `Signer::sign_transparent`.

use pczt::roles::signer::Signer as UpstreamSigner;
use secp256k1::{PublicKey, Secp256k1, SecretKey, SignOnly};
use zally_core::Network;
use zally_keys::SeedMaterial;
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_transparent::address::TransparentAddress;
use zcash_transparent::keys::{AccountPrivKey, NonHardenedChildIndex};

use crate::pczt_bytes::PcztBytes;
use crate::pczt_error::PcztError;

/// Maximum number of non-hardened child indices to enumerate per scope when matching
/// transparent inputs to wallet-owned keys.
///
/// ZIP-32 and BIP-44 use 20 as the conventional address gap limit; for fauzec's
/// single-receiver mining setup index 0 is the only one actually populated, but the wider
/// search keeps the signer general-purpose.
const TRANSPARENT_ADDRESS_GAP_LIMIT: u32 = 20;

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
    /// metadata aligns with the derived account's keys, and signs every transparent input
    /// whose `script_pubkey` matches a derived external- or internal-scope address within
    /// the gap limit. Returns [`PcztError::NoMatchingKeys`] when the seed cannot authorize
    /// any spend in the PCZT.
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
        let transparent_scripts: Vec<Vec<u8>> = parsed
            .transparent()
            .inputs()
            .iter()
            .map(|input| input.script_pubkey().clone())
            .collect();

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
        let transparent_total =
            sign_transparent_spends(&mut upstream, usk.transparent(), &transparent_scripts)?;
        if sapling_total == 0 && orchard_total == 0 && transparent_total == 0 {
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

fn sign_transparent_spends(
    signer: &mut UpstreamSigner,
    account_sk: &AccountPrivKey,
    script_pubkeys: &[Vec<u8>],
) -> Result<usize, PcztError> {
    if script_pubkeys.is_empty() {
        return Ok(0);
    }
    let secp = Secp256k1::signing_only();
    let candidates = derive_candidate_transparent_keys(account_sk)?;
    let mut authorized = 0_usize;
    for (index, script) in script_pubkeys.iter().enumerate() {
        if let Some(sk) = match_script_to_key(&secp, script, &candidates) {
            signer
                .sign_transparent(index, sk)
                .map_err(|err| PcztError::UpstreamFailed {
                    reason: format!("transparent spend {index} sign failed: {err:?}"),
                    is_retryable: false,
                })?;
            authorized += 1;
        }
    }
    Ok(authorized)
}

fn derive_candidate_transparent_keys(
    account_sk: &AccountPrivKey,
) -> Result<Vec<SecretKey>, PcztError> {
    let mut keys = Vec::with_capacity((TRANSPARENT_ADDRESS_GAP_LIMIT as usize) * 2);
    for index in 0..TRANSPARENT_ADDRESS_GAP_LIMIT {
        let address_index =
            NonHardenedChildIndex::from_index(index).ok_or_else(|| PcztError::UpstreamFailed {
                reason: format!("invalid non-hardened child index {index}"),
                is_retryable: false,
            })?;
        if let Ok(sk) = account_sk.derive_external_secret_key(address_index) {
            keys.push(sk);
        }
        if let Ok(sk) = account_sk.derive_internal_secret_key(address_index) {
            keys.push(sk);
        }
    }
    Ok(keys)
}

fn match_script_to_key<'a>(
    secp: &Secp256k1<SignOnly>,
    script_pubkey: &[u8],
    candidates: &'a [SecretKey],
) -> Option<&'a SecretKey> {
    for sk in candidates {
        let pubkey = PublicKey::from_secret_key(secp, sk);
        let address = TransparentAddress::from_pubkey(&pubkey);
        if p2pkh_script_pubkey(&address).as_slice() == script_pubkey {
            return Some(sk);
        }
    }
    None
}

fn p2pkh_script_pubkey(address: &TransparentAddress) -> Vec<u8> {
    let TransparentAddress::PublicKeyHash(hash) = address else {
        return Vec::new();
    };
    let mut script = Vec::with_capacity(25);
    script.extend_from_slice(&[0x76, 0xa9, 0x14]);
    script.extend_from_slice(hash);
    script.extend_from_slice(&[0x88, 0xac]);
    script
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

    #[test]
    fn p2pkh_script_pubkey_has_canonical_p2pkh_layout() {
        let hash = [0x11_u8; 20];
        let address = TransparentAddress::PublicKeyHash(hash);
        let script = p2pkh_script_pubkey(&address);
        assert_eq!(script.len(), 25);
        assert_eq!(&script[0..3], &[0x76, 0xa9, 0x14]);
        assert_eq!(&script[3..23], &hash);
        assert_eq!(&script[23..25], &[0x88, 0xac]);
    }

    #[test]
    fn derive_candidate_transparent_keys_returns_internal_and_external_per_index() {
        let mnemonic = Mnemonic::generate();
        let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
        let params = Network::Testnet.to_parameters();
        let usk = UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), zip32::AccountId::ZERO)
            .expect("derivation succeeds");
        let keys = derive_candidate_transparent_keys(usk.transparent())
            .expect("candidate derivation succeeds");
        // External + internal at each non-hardened index within the gap limit, so the
        // search space is 2 keys per index.
        assert_eq!(keys.len() as u32, TRANSPARENT_ADDRESS_GAP_LIMIT * 2);
    }
}
