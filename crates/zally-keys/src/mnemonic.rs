//! BIP-39 mnemonic.

use bip0039::{Count, English, Mnemonic as Bip0039Mnemonic};

/// BIP-39 mnemonic (24 words, 256 bits of entropy).
///
/// Wraps the underlying [`bip0039`] implementation. The Zally-owned shape lets the public
/// surface ([`crate::SeedSealing`], `Wallet::create`'s return tuple) reference one stable type
/// even when the BIP-39 crate dependency changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mnemonic {
    inner: Bip0039Mnemonic<English>,
}

impl Mnemonic {
    /// Generates a fresh 24-word mnemonic from the OS RNG.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            inner: Bip0039Mnemonic::<English>::generate(Count::Words24),
        }
    }

    /// Reconstructs a mnemonic from its written phrase.
    ///
    /// Validates wordlist membership and BIP-39 checksum.
    ///
    /// `not_retryable`: a phrase that fails today will fail every time.
    pub fn from_phrase(phrase: &str) -> Result<Self, MnemonicError> {
        Bip0039Mnemonic::<English>::from_phrase(phrase)
            .map(|inner| Self { inner })
            .map_err(|err| MnemonicError::InvalidPhrase {
                reason: err.to_string(),
            })
    }

    /// The space-separated mnemonic phrase. Treat as sensitive.
    #[must_use]
    pub fn as_phrase(&self) -> &str {
        self.inner.phrase()
    }

    /// Number of words in the mnemonic.
    #[must_use]
    pub fn word_count(&self) -> usize {
        self.inner.phrase().split_whitespace().count()
    }

    /// Derives the BIP-39 seed (PBKDF2-HMAC-SHA512, 2048 rounds).
    ///
    /// Returns the raw 64-byte seed. Used by [`crate::SeedMaterial::from_mnemonic`].
    pub(crate) fn to_seed_bytes(&self, passphrase: &str) -> [u8; 64] {
        self.inner.to_seed(passphrase)
    }
}

/// Error returned when [`Mnemonic`] construction fails.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum MnemonicError {
    /// The phrase is not a valid BIP-39 mnemonic.
    ///
    /// `not_retryable`: the caller must supply a valid phrase.
    #[error("mnemonic phrase is invalid: {reason}")]
    InvalidPhrase {
        /// Description of why the phrase failed validation.
        reason: String,
    },
}

impl MnemonicError {
    /// Whether the same input may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::InvalidPhrase { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mnemonic_generate_yields_24_words() {
        let m = Mnemonic::generate();
        assert_eq!(m.word_count(), 24);
    }

    #[test]
    fn mnemonic_generate_then_parse_round_trip() -> Result<(), MnemonicError> {
        let m1 = Mnemonic::generate();
        let m2 = Mnemonic::from_phrase(m1.as_phrase())?;
        assert_eq!(m1.as_phrase(), m2.as_phrase());
        Ok(())
    }

    #[test]
    fn mnemonic_invalid_phrase_rejected() {
        let outcome = Mnemonic::from_phrase("not a valid mnemonic at all");
        assert!(matches!(outcome, Err(MnemonicError::InvalidPhrase { .. })));
    }
}
