//! Operator-facing wallet error.

use zally_chain::{ChainSourceError, FailurePosture, SubmitterError};
use zally_core::{BlockHeight, Network, Zatoshis};
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
    Storage(StorageError),

    /// Key derivation failed.
    #[error("key derivation error: {0}")]
    KeyDerivation(#[from] KeyDerivationError),

    /// `Wallet::open` was called but no sealed seed exists.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]: a human provisions the seed (or runs
    /// `Wallet::create` for first-time bootstrap); no retry conjures one.
    #[error("no sealed seed found for wallet")]
    NoSealedSeed,

    /// `Wallet::create` was called but an account already exists.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]: switch to `Wallet::open`.
    #[error("an account already exists at this wallet location")]
    AccountAlreadyExists,

    /// The unsealed seed does not match any account in storage.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]: a seed and storage mismatch needs
    /// investigation before any call can succeed.
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
        /// Amount the caller asked to send.
        requested_zat: Zatoshis,
        /// Spendable balance reported by storage at proposal time.
        spendable_zat: Zatoshis,
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

    /// The submission did not produce an `Accepted`, `Duplicate`, or `Queued` outcome. The
    /// submitter returned `Rejected` with the carried typed reason and operator-facing
    /// detail.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]: retrying the same bytes will not change
    /// the outcome.
    #[error("submission rejected ({reason:?}): {detail}")]
    SubmissionRejected {
        /// Typed rejection reason from the upstream node.
        reason: zally_chain::RejectionReason,
        /// Operator-facing detail describing the rejection.
        detail: String,
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

    /// The caller-supplied `target_expiry_height` is at or below the wallet's observed chain
    /// tip, so any signed transaction would already be past its allowed expiry window.
    ///
    /// Posture: [`FailurePosture::NotRetryable`] until the caller picks a fresh height
    /// (typically after observing a new tip).
    #[error(
        "target_expiry_height={target:?} is at or below the wallet's observed tip={chain_tip:?}"
    )]
    TargetExpiryStale {
        /// Height the caller asked the wallet to commit to.
        target: BlockHeight,
        /// Chain tip the wallet had observed at the time of the call.
        chain_tip: BlockHeight,
    },

    /// The signed transaction's `expiry_height` does not match the caller-supplied
    /// `target_expiry_height`.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]: indicates a bug in the PCZT Updater
    /// step or an unexpected upstream mutation; the caller should not retry the same plan
    /// blindly.
    #[error(
        "signed transaction expiry_height={signed:?} does not match target_expiry_height={target:?}"
    )]
    TargetExpiryMismatch {
        /// Height the caller asked the wallet to commit to.
        target: BlockHeight,
        /// Height the wallet actually signed.
        signed: BlockHeight,
    },

    /// The wallet's full note-commitment tree root disagrees with the chain's tree state at
    /// `height`: the wallet assembled a corrupt tree, and any spend anchored on it would be
    /// rejected by the network. The sync driver treats this as its cue to rewind below the
    /// divergence and ultimately rebuild derived state from the birthday.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]: re-issuing the same scan reproduces the
    /// same corrupt tree.
    #[error("wallet commitment-tree roots diverge from the chain at height {height:?}")]
    TreeRootsDiverged {
        /// Height whose chain tree state disagreed with the wallet's roots.
        height: BlockHeight,
    },

    /// The sync-driver task panicked. Surfaced only by [`crate::SyncHandle::close`]; the
    /// driver task never fails while its handle is alive.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]: a panic indicates a Zally bug or an
    /// upstream invariant violation that needs investigation before a new driver is
    /// started.
    #[error("sync driver task panicked: {reason}")]
    SyncDriverFailed {
        /// Underlying join-error description.
        reason: String,
    },
}

impl WalletError {
    /// Operator-facing posture describing what the consumer may do.
    #[must_use]
    pub fn posture(&self) -> FailurePosture {
        match self {
            Self::Sealing(e) => bool_to_posture(e.is_retryable()),
            Self::Storage(e) => e.posture(),
            Self::KeyDerivation(e) => bool_to_posture(e.is_retryable()),
            Self::Pczt(e) => bool_to_posture(e.is_retryable()),
            Self::ChainSource(e) => e.posture(),
            Self::Submitter(e) => e.posture(),
            Self::CircuitBroken { .. } => FailurePosture::Retryable,
            Self::NetworkMismatch { .. }
            | Self::NoSealedSeed
            | Self::AccountNotFound
            | Self::TargetExpiryMismatch { .. }
            | Self::SyncDriverFailed { .. } => FailurePosture::RequiresOperator,
            Self::AccountAlreadyExists
            | Self::MemoOnTransparentRecipient
            | Self::ShieldedInputsOnTexRecipient
            | Self::InsufficientBalance { .. }
            | Self::PaymentRequestParseFailed { .. }
            | Self::ProposalRejected { .. }
            | Self::SubmissionRejected { .. }
            | Self::TargetExpiryStale { .. }
            | Self::TreeRootsDiverged { .. } => FailurePosture::NotRetryable,
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

/// Maps the two-state retry posture exposed by boundary errors that still expose
/// `is_retryable: bool` (`SealingError`, `KeyDerivationError`, `PcztError`) into
/// [`FailurePosture`].
const fn bool_to_posture(is_retryable: bool) -> FailurePosture {
    if is_retryable {
        FailurePosture::Retryable
    } else {
        FailurePosture::NotRetryable
    }
}

impl From<StorageError> for WalletError {
    fn from(err: StorageError) -> Self {
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "the named arms cover every variant whose translation is non-trivial; \
                      the wildcard preserves the storage error verbatim"
        )]
        match err {
            StorageError::AccountNotFound => Self::AccountNotFound,
            StorageError::AccountAlreadyExists => Self::AccountAlreadyExists,
            StorageError::InsufficientFunds {
                required_zat,
                available_zat,
            } => Self::InsufficientBalance {
                requested_zat: required_zat,
                spendable_zat: available_zat,
            },
            StorageError::ProposalBuildFailed { reason, .. } => Self::ProposalRejected { reason },
            other => Self::Storage(other),
        }
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
                requested_zat: Zatoshis::try_from(10_u64).unwrap_or(Zatoshis::zero()),
                spendable_zat: Zatoshis::zero(),
            },
            WalletError::PaymentRequestParseFailed { reason: "x".into() },
            WalletError::ProposalRejected { reason: "x".into() },
            WalletError::SubmissionRejected {
                reason: zally_chain::RejectionReason::Unknown,
                detail: "x".into(),
            },
            WalletError::Pczt(PcztError::NoMatchingKeys),
            WalletError::CircuitBroken { operation: "test" },
            WalletError::TargetExpiryStale {
                target: BlockHeight::from(10),
                chain_tip: BlockHeight::from(20),
            },
            WalletError::TargetExpiryMismatch {
                target: BlockHeight::from(10),
                signed: BlockHeight::from(11),
            },
            WalletError::TreeRootsDiverged {
                height: BlockHeight::from(12),
            },
            WalletError::SyncDriverFailed { reason: "x".into() },
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
    fn provisioning_dead_ends_require_operator() {
        for err in [WalletError::NoSealedSeed, WalletError::AccountNotFound] {
            assert_eq!(err.posture(), FailurePosture::RequiresOperator);
            assert!(!err.is_retryable());
        }
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
    fn sync_driver_failed_requires_operator() {
        let err = WalletError::SyncDriverFailed {
            reason: "panicked".into(),
        };
        assert_eq!(err.posture(), FailurePosture::RequiresOperator);
        assert!(!err.is_retryable());
    }

    #[test]
    fn tree_roots_diverged_is_not_retryable() {
        let err = WalletError::TreeRootsDiverged {
            height: BlockHeight::from(7),
        };
        assert_eq!(err.posture(), FailurePosture::NotRetryable);
        assert!(!err.is_retryable());
    }
}
