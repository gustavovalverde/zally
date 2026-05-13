//! Operator-facing wallet error.

use zally_core::Network;
use zally_keys::{KeyDerivationError, SealingError};
use zally_pczt::PcztError;
use zally_storage::StorageError;

/// Error returned by [`crate::Wallet`] operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WalletError {
    /// A `SeedSealing` operation failed.
    #[error("seed sealing error: {0}")]
    Sealing(#[from] SealingError),

    /// A `WalletStorage` operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Key derivation failed.
    #[error("key derivation error: {0}")]
    KeyDerivation(#[from] KeyDerivationError),

    /// `Wallet::open` was called but no sealed seed exists.
    ///
    /// `not_retryable`: switch to `Wallet::create` for first-time bootstrap.
    #[error("no sealed seed found for wallet")]
    NoSealedSeed,

    /// `Wallet::create` was called but an account already exists.
    ///
    /// `not_retryable`: switch to `Wallet::open`.
    #[error("an account already exists at this wallet location")]
    AccountAlreadyExists,

    /// The unsealed seed does not match any account in storage.
    ///
    /// `not_retryable`.
    #[error("no account in storage matches the unsealed seed")]
    AccountNotFound,

    /// The storage's network does not match the wallet's requested network.
    ///
    /// `requires_operator`: configuration mismatch.
    #[error("network mismatch: storage={storage:?}, requested={requested:?}")]
    NetworkMismatch {
        /// Network the storage was opened for.
        storage: Network,
        /// Network passed to `Wallet::create` or `Wallet::open`.
        requested: Network,
    },

    /// A chain-source operation failed. The posture follows the underlying chain-source
    /// error.
    #[error("chain source error: {reason}")]
    ChainSource {
        /// Underlying error description.
        reason: String,
        /// Whether retrying the same call may succeed.
        is_retryable: bool,
    },

    /// A submitter operation failed.
    #[error("submitter error: {0}")]
    Submitter(#[from] zally_chain::SubmitterError),

    /// A memo was provided for a transparent recipient.
    ///
    /// `not_retryable`: ZIP-302 prohibits memos on transparent outputs.
    #[error("memos are not permitted on transparent recipients (ZIP-302)")]
    MemoOnTransparentRecipient,

    /// A TEX recipient was paid from a proposal containing shielded inputs.
    ///
    /// `not_retryable`: ZIP-320 requires an all-transparent input set when paying a TEX
    /// recipient.
    #[error("TEX recipients (ZIP-320) require an all-transparent input set")]
    ShieldedInputsOnTexRecipient,

    /// The wallet does not have enough spendable balance for the requested send.
    ///
    /// `not_retryable` until balance is replenished.
    #[error("insufficient spendable balance: requested {requested_zat}, spendable {spendable_zat}")]
    InsufficientBalance {
        /// Amount the caller asked to send, in zatoshis.
        requested_zat: u64,
        /// Spendable balance reported by storage, in zatoshis.
        spendable_zat: u64,
    },

    /// A ZIP-321 payment-request URI could not be parsed.
    ///
    /// `not_retryable`: the caller must fix the URI.
    #[error("payment request parse failed: {reason}")]
    PaymentRequestParseFailed {
        /// Underlying parser error.
        reason: String,
    },

    /// The proposal could not be built (e.g., unsupported recipient combination, missing
    /// anchor, fee constraint).
    ///
    /// `not_retryable` for malformed input; `requires_operator` for missing chain state.
    #[error("proposal rejected: {reason}")]
    ProposalRejected {
        /// Underlying error description.
        reason: String,
    },

    /// The submission did not produce an `Accepted` or `Duplicate` outcome. The submitter
    /// returned `Rejected` with the carried reason.
    ///
    /// `not_retryable`: retrying the same bytes will not change the outcome.
    #[error("submission rejected: {reason}")]
    SubmissionRejected {
        /// Reason carried by the submitter.
        reason: String,
    },

    /// A PCZT role (Creator, Signer, Combiner, Extractor) returned an error.
    ///
    /// Posture follows the underlying [`PcztError`].
    #[error("pczt error: {0}")]
    Pczt(#[from] PcztError),

    /// The wallet's circuit breaker is open and short-circuited the call. The breaker
    /// re-closes after its cooldown expires or after a half-open probe succeeds.
    ///
    /// `retryable`: callers may try again after the cooldown.
    #[error("wallet circuit breaker is open; failures exceeded the configured threshold")]
    CircuitBroken {
        /// Operation that observed the open circuit.
        operation: &'static str,
    },
}

impl WalletError {
    /// Whether the same call may succeed on retry.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Sealing(e) => e.is_retryable(),
            Self::Storage(e) => e.is_retryable(),
            Self::KeyDerivation(e) => e.is_retryable(),
            Self::ChainSource { is_retryable, .. } => *is_retryable,
            Self::Submitter(e) => e.is_retryable(),
            Self::Pczt(e) => e.is_retryable(),
            Self::CircuitBroken { .. } => true,
            Self::NoSealedSeed
            | Self::AccountAlreadyExists
            | Self::AccountNotFound
            | Self::NetworkMismatch { .. }
            | Self::MemoOnTransparentRecipient
            | Self::ShieldedInputsOnTexRecipient
            | Self::InsufficientBalance { .. }
            | Self::PaymentRequestParseFailed { .. }
            | Self::ProposalRejected { .. }
            | Self::SubmissionRejected { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_error_retryable_match_complete() {
        let variants = [
            WalletError::Sealing(SealingError::NoSealedSeed),
            WalletError::Storage(StorageError::NotOpened),
            WalletError::KeyDerivation(KeyDerivationError::DerivationFailed { reason: "x".into() }),
            WalletError::NoSealedSeed,
            WalletError::AccountAlreadyExists,
            WalletError::AccountNotFound,
            WalletError::NetworkMismatch {
                storage: Network::Mainnet,
                requested: Network::Testnet,
            },
            WalletError::ChainSource {
                reason: "x".into(),
                is_retryable: true,
            },
            WalletError::Submitter(zally_chain::SubmitterError::Unavailable { reason: "x".into() }),
            WalletError::MemoOnTransparentRecipient,
            WalletError::ShieldedInputsOnTexRecipient,
            WalletError::InsufficientBalance {
                requested_zat: 10,
                spendable_zat: 0,
            },
            WalletError::PaymentRequestParseFailed { reason: "x".into() },
            WalletError::ProposalRejected { reason: "x".into() },
            WalletError::SubmissionRejected { reason: "x".into() },
            WalletError::Pczt(PcztError::NoMatchingKeys),
            WalletError::CircuitBroken { operation: "test" },
        ];
        for e in variants {
            let _ = e.is_retryable();
        }
    }
}
