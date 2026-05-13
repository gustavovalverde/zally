//! Zeroizing wrapper around raw seed bytes.

use secrecy::{ExposeSecret, Secret};
use zeroize::Zeroize;

use crate::mnemonic::Mnemonic;

/// Zeroizing wrapper around raw seed bytes (32-252 bytes per ZIP-32).
///
/// Constructed from a [`Mnemonic`] via [`SeedMaterial::from_mnemonic`]. The underlying buffer
/// is zeroized on drop via [`secrecy::Secret`].
pub struct SeedMaterial(Secret<Vec<u8>>);

impl SeedMaterial {
    /// Derives seed bytes from a 24-word [`Mnemonic`] with the given passphrase.
    ///
    /// Returns the standard BIP-39 PBKDF2-HMAC-SHA512 derivation (2048 rounds) yielding a
    /// 64-byte seed. Pass `""` for the standard "no passphrase" derivation.
    #[must_use]
    pub fn from_mnemonic(mnemonic: &Mnemonic, passphrase: &str) -> Self {
        let mut bytes = mnemonic.to_seed_bytes(passphrase);
        let owned = bytes.to_vec();
        bytes.zeroize();
        Self(Secret::new(owned))
    }

    /// Wraps raw seed bytes after validating the ZIP-32 length range (32-252 bytes).
    ///
    /// Most operators should use [`SeedMaterial::from_mnemonic`] instead; this constructor is
    /// the seam for [`crate::SeedSealing`] implementations that unseal raw bytes and for
    /// testkit fixtures.
    pub fn from_raw_bytes(bytes: Vec<u8>) -> Result<Self, SeedMaterialError> {
        let byte_count = bytes.len();
        if !(32..=252).contains(&byte_count) {
            return Err(SeedMaterialError::InvalidLength { byte_count });
        }
        Ok(Self(Secret::new(bytes)))
    }

    /// Returns the raw seed bytes for use with librustzcash derivation calls.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        self.0.expose_secret().as_slice()
    }

    /// Returns the seed byte count.
    #[must_use]
    pub fn byte_count(&self) -> usize {
        self.0.expose_secret().len()
    }
}

/// Error returned when [`SeedMaterial`] construction fails.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SeedMaterialError {
    /// The decoded seed has an invalid length.
    ///
    /// `requires_operator`: the sealed material does not conform to ZIP-32.
    #[error("decoded seed length is {byte_count}; ZIP-32 requires 32-252 bytes")]
    InvalidLength {
        /// Length of the rejected seed material.
        byte_count: usize,
    },
}

impl SeedMaterialError {
    /// Whether the same input may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::InvalidLength { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_material_from_mnemonic_yields_64_bytes() {
        let mnemonic = Mnemonic::generate();
        let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
        assert_eq!(seed.byte_count(), 64);
        assert_eq!(seed.expose_secret().len(), 64);
    }

    #[test]
    fn seed_material_too_short_rejected() {
        let outcome = SeedMaterial::from_raw_bytes(vec![0_u8; 16]);
        assert!(matches!(
            outcome,
            Err(SeedMaterialError::InvalidLength { byte_count: 16 })
        ));
    }

    #[test]
    fn seed_material_too_long_rejected() {
        let outcome = SeedMaterial::from_raw_bytes(vec![0_u8; 500]);
        assert!(matches!(
            outcome,
            Err(SeedMaterialError::InvalidLength { byte_count: 500 })
        ));
    }
}
