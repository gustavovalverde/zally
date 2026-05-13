//! UFVK derivation.

use zally_core::Network;
use zcash_keys::keys::{UnifiedFullViewingKey, UnifiedSpendingKey};

use crate::seed_material::SeedMaterial;

/// Derives a [`UnifiedFullViewingKey`] for `account_index` on `network`.
///
/// Internally derives the [`UnifiedSpendingKey`] from the seed and converts it to its UFVK.
/// The spending key falls out of scope before this function returns; the inner pool-specific
/// secret keys zeroize on drop.
///
/// `not_retryable`: derivation is deterministic for the same inputs.
pub fn derive_ufvk(
    network: Network,
    seed: &SeedMaterial,
    account_index: zip32::AccountId,
) -> Result<UnifiedFullViewingKey, KeyDerivationError> {
    let params = network.to_parameters();
    let usk = UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), account_index).map_err(
        |err| KeyDerivationError::DerivationFailed {
            reason: err.to_string(),
        },
    )?;
    Ok(usk.to_unified_full_viewing_key())
}

/// Error returned when [`derive_ufvk`] fails.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyDerivationError {
    /// The seed does not yield a valid UFVK at the requested account index.
    ///
    /// `not_retryable`: derivation is deterministic.
    #[error("unified key derivation failed: {reason}")]
    DerivationFailed {
        /// Underlying error description.
        reason: String,
    },
}

impl KeyDerivationError {
    /// Whether the same call may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::DerivationFailed { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mnemonic::Mnemonic;

    #[test]
    fn derive_ufvk_deterministic() -> Result<(), KeyDerivationError> {
        let mnemonic = Mnemonic::generate();
        let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
        let network = Network::regtest_all_at_genesis();
        let index = zip32::AccountId::ZERO;

        let ufvk1 = derive_ufvk(network, &seed, index)?;
        let ufvk2 = derive_ufvk(network, &seed, index)?;
        assert_eq!(
            ufvk1.encode(&network.to_parameters()),
            ufvk2.encode(&network.to_parameters())
        );
        Ok(())
    }
}
