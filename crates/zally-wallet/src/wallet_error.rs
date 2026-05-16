//! Operator-facing wallet error.

use zally_chain::{ChainSourceError, FailurePosture, SubmitterError};
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
    /// Posture: [`FailurePosture::NotRetryable`]: switch to `Wallet::create` for first-time
    /// bootstrap.
    #[error("no sealed seed found for wallet")]
    NoSealedSeed,

    /// `Wallet::create` was called but an account already exists.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]: switch to `Wallet::open`.
    #[error("an account already exists at this wallet location")]
    AccountAlreadyExists,

    /// The unsealed seed does not match any account in storage.
    ///
    /// Posture: [`FailurePosture::NotRetryable`].
    #[error("no account in storage matches the unsealed seed")]
    AccountNotFound,

    /// The storage's network does not match the wallet's requested network.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]: configuration mismatch.
    #[error("network mismatch: storage={storage:?}, requested={requested:?}")]
    NetworkMismatch {
        /// Network the storage was opened for.
        storage: Network,
        /// Network passed to `Wallet::create` or `Wallet::open`.
        requested: Network,
    },

    /// A chain-source operation failed. Posture follows the inner [`ChainSourceError`].
    #[error("chain source error: {0}")]
    ChainSource(#[from] ChainSourceError),

    /// A submitter operation failed. Posture follows the inner [`SubmitterError`].
    #[error("submitter error: {0}")]
    Submitter(#[from] SubmitterError),

    /// A memo was provided for a transparent recipient.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]: ZIP-302 prohibits memos on transparent
    /// outputs.
    #[error("memos are not permitted on transparent recipients (ZIP-302)")]
    MemoOnTransparentRecipient,

    /// A TEX recipient was paid from a proposal containing shielded inputs.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]: ZIP-320 requires an all-transparent input set
    /// when paying a TEX recipient.
    #[error("TEX recipients (ZIP-320) require an all-transparent input set")]
    ShieldedInputsOnTexRecipient,

    /// The wallet does not have enough spendable balance for the requested send.
    ///
    /// Posture: [`FailurePosture::NotRetryable`] until balance is replenished.
    #[error("insufficient spendable balance: requested {requested_zat}, spendable {spendable_zat}")]
    InsufficientBalance {
        /// Amount the caller asked to send, in zatoshis.
        requested_zat: u64,
        /// Spendable balance reported by storage, in zatoshis.
        spendable_zat: u64,
    },

    /// A ZIP-321 payment-request URI could not be parsed.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]: the caller must fix the URI.
    #[error("payment request parse failed: {reason}")]
    PaymentRequestParseFailed {
        /// Underlying parser error.
        reason: String,
    },

    /// The proposal could not be built (e.g., unsupported recipient combination, missing
    /// anchor, fee constraint).
    ///
    /// Posture: [`FailurePosture::NotRetryable`] for malformed input.
    #[error("proposal rejected: {reason}")]
    ProposalRejected {
        /// Underlying error description.
        reason: String,
    },

    /// The submission did not produce an `Accepted` or `Duplicate` outcome. The submitter
    /// returned `Rejected` with the carried reason.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]: retrying the same bytes will not change
    /// the outcome.
    #[error("submission rejected: {reason}")]
    SubmissionRejected {
        /// Reason carried by the submitter.
        reason: String,
    },

    /// A PCZT role (Creator, Prover, Signer, Combiner, Extractor) returned an error.
    ///
    /// Posture follows the underlying [`PcztError`].
    #[error("pczt error: {0}")]
    Pczt(#[from] PcztError),

    /// The wallet's circuit breaker is open and short-circuited the call. The breaker
    /// re-closes after its cooldown expires or after a half-open probe succeeds.
    ///
    /// Posture: [`FailurePosture::Retryable`]: callers may try again after the cooldown.
    #[error("wallet circuit breaker is open; failures exceeded the configured threshold")]
    CircuitBroken {
        /// Operation that observed the open circuit.
        operation: &'static str,
    },

    /// The sync-driver task failed outside the wallet sync operation itself.
    ///
    /// Posture carries the operator-facing classification. A runtime cancellation surfaces as
    /// [`FailurePosture::Retryable`]; a panic in the driver task surfaces as
    /// [`FailurePosture::RequiresOperator`] (the panic indicates a Zally bug or upstream
    /// invariant violation that needs investigation before a new driver is started).
    #[error("sync driver failed: {reason}")]
    SyncDriverFailed {
        /// Underlying task failure description.
        reason: String,
        /// Operator-facing posture for this failure.
        posture: FailurePosture,
    },
}

impl WalletError {
    /// Operator-facing posture describing what the consumer may do.
    #[must_use]
    pub fn posture(&self) -> FailurePosture {
        match self {
            Self::Sealing(e) => bool_to_posture(e.is_retryable()),
            Self::Storage(e) => bool_to_posture(e.is_retryable()),
            Self::KeyDerivation(e) => bool_to_posture(e.is_retryable()),
            Self::Pczt(e) => bool_to_posture(e.is_retryable()),
            Self::ChainSource(e) => e.posture(),
            Self::Submitter(e) => e.posture(),
            Self::SyncDriverFailed { posture, .. } => *posture,
            Self::CircuitBroken { .. } => FailurePosture::Retryable,
            Self::NetworkMismatch { .. } => FailurePosture::RequiresOperator,
            Self::NoSealedSeed
            | Self::AccountAlreadyExists
            | Self::AccountNotFound
            | Self::MemoOnTransparentRecipient
            | Self::ShieldedInputsOnTexRecipient
            | Self::InsufficientBalance { .. }
            | Self::PaymentRequestParseFailed { .. }
            | Self::ProposalRejected { .. }
            | Self::SubmissionRejected { .. } => FailurePosture::NotRetryable,
        }
    }

    /// Convenience: `true` when the same call may succeed on retry.
    ///
    /// Equivalent to `self.posture().allows_retry()`.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.posture().allows_retry()
    }
}

/// Maps the two-state retry posture exposed by boundary errors into [`FailurePosture`].
///
/// `StorageError`, `SealingError`, `KeyDerivationError`, and `PcztError` keep a boolean
/// signal because their retry semantics never need to distinguish "operator must act" from
/// "caller fix needed"; the wallet-side mapping picks the conservative non-retryable
/// variant for any `false`.
const fn bool_to_posture(is_retryable: bool) -> FailurePosture {
    if is_retryable {
        FailurePosture::Retryable
    } else {
        FailurePosture::NotRetryable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_error_posture_match_complete() {
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
            WalletError::ChainSource(ChainSourceError::Unavailable { reason: "x".into() }),
            WalletError::Submitter(SubmitterError::Unavailable { reason: "x".into() }),
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
            WalletError::SyncDriverFailed {
                reason: "x".into(),
                posture: FailurePosture::Retryable,
            },
        ];
        for e in variants {
            let _ = e.posture();
            let _ = e.is_retryable();
        }
    }

    #[test]
    fn network_mismatch_requires_operator() {
        let err = WalletError::NetworkMismatch {
            storage: Network::Mainnet,
            requested: Network::Testnet,
        };
        assert_eq!(err.posture(), FailurePosture::RequiresOperator);
        assert!(!err.is_retryable());
    }

    #[test]
    fn chain_source_posture_delegates_to_inner() {
        let err = WalletError::ChainSource(ChainSourceError::MalformedCompactBlock {
            block_height: zally_core::BlockHeight::from(1),
            reason: "x".into(),
        });
        assert_eq!(err.posture(), FailurePosture::RequiresOperator);
    }

    #[test]
    fn sync_driver_failed_posture_is_carried_through() {
        let err = WalletError::SyncDriverFailed {
            reason: "panicked".into(),
            posture: FailurePosture::RequiresOperator,
        };
        assert_eq!(err.posture(), FailurePosture::RequiresOperator);
        assert!(!err.is_retryable());
    }
}
