//! `SeedSealing` trait and `SealingError` enum.

use async_trait::async_trait;

use crate::seed_material::{SeedMaterial, SeedMaterialError};

/// The sealing implementation behind a wallet handle.
///
/// Returned by [`SeedSealing::kind`] so the wallet capability descriptor can advertise the
/// in-use sealing flavour without `std::any::type_name::<S>()` introspection.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SealingKind {
    /// Age-encrypted file sealing (`zally_keys::AgeFileSealing`).
    AgeFile,
    /// In-memory sealing (`zally_testkit::InMemorySealing`). Tests only.
    InMemory,
    /// Plaintext seed storage. Available only behind the `unsafe_plaintext_seed` feature.
    Plaintext,
    /// A custom sealing implementation provided by the operator.
    Custom,
}

/// The security posture the sealing implementation provides at runtime.
///
/// Orthogonal to [`SealingKind`]: a `SealingKind::AgeFile` impl is `Dev` posture for a
/// developer-machine identity but `Kms`/`Hsm` posture when the identity itself is
/// produced by a hosted KMS or hardware module. Returned by [`SeedSealing::posture`]
/// so the wallet runtime can refuse to start with `Dev` posture in production deploys
/// (D-13 of Proposal-0003).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SealingPosture {
    /// Developer-machine posture. Acceptable for local dev and CI; the runtime must
    /// refuse to serve production traffic in this posture without an explicit
    /// operator override.
    Dev,
    /// Hardware-security-module-backed posture. Seed material never leaves the HSM
    /// boundary in plaintext.
    Hsm,
    /// Cloud KMS-backed posture (AWS KMS, GCP KMS, `HashiCorp` Vault, etc.). Seed
    /// material is unwrapped within the KMS boundary on demand.
    Kms,
}

/// Trait for at-rest seed encryption.
///
/// Implementations hold the sealed seed and expose [`SeedSealing::seal_seed`] /
/// [`SeedSealing::unseal_seed`]. The trait is network-agnostic: a sealed seed carries no chain
/// state. Network binding lives on the wallet handle and the storage backend.
///
/// All methods take `&self`; implementations own their concurrency strategy (interior mutex,
/// connection pool, etc.). Callers hold the trait object via `Box<dyn SeedSealing>` inside the
/// wallet, with no external mutex layer required.
#[async_trait]
pub trait SeedSealing: Send + Sync + 'static {
    /// Returns the sealing flavour for the wallet capability descriptor. Default is
    /// [`SealingKind::Custom`]; first-party implementations override.
    fn kind(&self) -> SealingKind {
        SealingKind::Custom
    }

    /// Returns the security posture the sealing impl provides at runtime.
    ///
    /// The default is conservative: [`SealingPosture::Dev`]. First-party HSM and
    /// KMS adapters override. The wallet runtime gates production startup on the
    /// posture value (D-13 of Proposal-0003); a runtime that boots with `Dev`
    /// posture without an explicit operator override is a configuration bug.
    fn posture(&self) -> SealingPosture {
        SealingPosture::Dev
    }

    /// Encrypts and persists `seed`. Idempotent: calling twice with the same seed replaces the
    /// prior sealed copy.
    ///
    /// `retryable` on transient I/O. `requires_operator` on key material errors.
    async fn seal_seed(&self, seed: &SeedMaterial) -> Result<(), SealingError>;

    /// Decrypts and returns the sealed seed material.
    ///
    /// `retryable` on transient I/O. `requires_operator` on integrity failure.
    /// `not_retryable` on [`SealingError::NoSealedSeed`].
    async fn unseal_seed(&self) -> Result<SeedMaterial, SealingError>;
}

/// Error returned by [`SeedSealing`] operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SealingError {
    /// Reading the seed file failed.
    ///
    /// `retryable`: transient I/O may self-heal.
    #[error("seed file read failed: {reason}")]
    ReadFailed {
        /// Underlying error description.
        reason: String,
    },

    /// Writing the seed file failed.
    ///
    /// `retryable`: transient I/O may self-heal.
    #[error("seed file write failed: {reason}")]
    WriteFailed {
        /// Underlying error description.
        reason: String,
    },

    /// No sealed seed exists at the configured path.
    ///
    /// `not_retryable`: switch to `Wallet::create` for first-time bootstrap.
    #[error("no sealed seed found at the configured path")]
    NoSealedSeed,

    /// The age identity file is missing or corrupt.
    ///
    /// `requires_operator`.
    #[error("age identity error: {reason}")]
    AgeIdentityFailed {
        /// Underlying error description.
        reason: String,
    },

    /// Age decryption failed. The sealed file may be corrupt or the identity may not match.
    ///
    /// `requires_operator`.
    #[error("age decryption failed: {reason}")]
    DecryptionFailed {
        /// Underlying error description.
        reason: String,
    },

    /// Age encryption failed.
    ///
    /// `requires_operator`.
    #[error("age encryption failed: {reason}")]
    EncryptionFailed {
        /// Underlying error description.
        reason: String,
    },

    /// The decoded seed length is invalid per ZIP-32.
    ///
    /// `requires_operator`: the sealed file stores invalid material.
    #[error("unsealed seed length is {byte_count}; ZIP-32 requires 32-252 bytes")]
    InvalidSeedLength {
        /// Length of the decoded seed.
        byte_count: usize,
    },

    /// A background task panicked or was cancelled.
    ///
    /// `retryable`.
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed {
        /// Underlying error description.
        reason: String,
    },
}

impl From<SeedMaterialError> for SealingError {
    fn from(err: SeedMaterialError) -> Self {
        match err {
            SeedMaterialError::InvalidLength { byte_count } => {
                Self::InvalidSeedLength { byte_count }
            }
        }
    }
}

impl SealingError {
    /// Whether the same call may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::ReadFailed { .. }
            | Self::WriteFailed { .. }
            | Self::BlockingTaskFailed { .. } => true,
            Self::NoSealedSeed
            | Self::AgeIdentityFailed { .. }
            | Self::DecryptionFailed { .. }
            | Self::EncryptionFailed { .. }
            | Self::InvalidSeedLength { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealing_error_retryable_match_complete() {
        let variants: [SealingError; 8] = [
            SealingError::ReadFailed { reason: "x".into() },
            SealingError::WriteFailed { reason: "x".into() },
            SealingError::NoSealedSeed,
            SealingError::AgeIdentityFailed { reason: "x".into() },
            SealingError::DecryptionFailed { reason: "x".into() },
            SealingError::EncryptionFailed { reason: "x".into() },
            SealingError::InvalidSeedLength { byte_count: 0 },
            SealingError::BlockingTaskFailed { reason: "x".into() },
        ];
        for e in variants {
            let _ = e.is_retryable();
        }
    }
}
