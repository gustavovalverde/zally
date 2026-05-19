//! Errors returned by [`WalletStorage`](crate::WalletStorage) operations.

use zally_core::{BlockHeight, FailurePosture, Zatoshis};

/// Error returned by [`WalletStorage`](crate::WalletStorage) operations.
///
/// Canonical error type for every storage backend. Backends with a different native error
/// translate inside their impl rather than exposing an associated `Error` type on the
/// trait. Every variant carries an explicit [`FailurePosture`] so the wallet boundary does
/// not have to infer one from a `bool` or a stringified reason.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// The caller invoked an operation before [`crate::WalletStorage::open_or_create`].
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; call `open_or_create` first.
    #[error("wallet storage was not opened; call open_or_create first")]
    NotOpened,

    /// Schema migration failed.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]; a schema mismatch needs manual
    /// intervention.
    #[error("wallet database migration failed: {reason}")]
    MigrationFailed {
        /// Underlying error description.
        reason: String,
    },

    /// A sqlite operation failed. The posture carries the inferred retry classification: a
    /// busy/locked database is `Retryable`; a missing table or invalid schema is
    /// `RequiresOperator`; a malformed query is `NotRetryable`.
    #[error("sqlite error ({posture:?}): {reason}")]
    SqliteFailed {
        /// Underlying error description.
        reason: String,
        /// Operator-facing posture for this failure.
        posture: FailurePosture,
    },

    /// The requested account was not found.
    ///
    /// Posture: [`FailurePosture::NotRetryable`].
    #[error("account not found in wallet")]
    AccountNotFound,

    /// An account already exists in this wallet.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; call
    /// [`crate::WalletStorage::find_account_for_seed`] or `Wallet::open` instead. Zally
    /// holds one account per wallet.
    #[error("an account already exists in this wallet; one account per wallet")]
    AccountAlreadyExists,

    /// A background task panicked or was cancelled.
    ///
    /// Posture: [`FailurePosture::Retryable`]; the tokio runtime may accept the task on
    /// retry.
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed {
        /// Underlying error description.
        reason: String,
    },

    /// Key derivation inside a storage operation failed.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; derivation is deterministic.
    #[error("key derivation failed: {reason}")]
    KeyDerivationFailed {
        /// Underlying error description.
        reason: String,
    },

    /// Sapling prover params are not available on disk.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]; download the Sapling spend/output
    /// parameters into the platform-default location (`~/.local/share/ZcashParams/` on
    /// macOS; `~/.zcash-params/` on Linux). The upstream
    /// `zcash_proofs::download_sapling_parameters` helper or the canonical `zcash-params`
    /// distribution bucket are the sources.
    #[error(
        "Sapling prover parameters are not available; install sapling-spend.params and \
         sapling-output.params into the platform-default location"
    )]
    ProverUnavailable,

    /// The supplied `IdempotencyKey` already maps to a different `TxId` in the ledger.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; the wallet layer surfaces the prior `TxId`
    /// to the caller instead of overwriting the ledger entry.
    #[error(
        "idempotency key already bound to a different transaction; \
         prior tx_id was recorded for this key"
    )]
    IdempotencyKeyConflict,

    /// `scan_blocks` rejected the batch because the chain rolled back. The wallet's view at
    /// `at_height` does not match the parent hash of the next block the chain source served.
    ///
    /// Posture: [`FailurePosture::Retryable`]; callers truncate the wallet to before
    /// `at_height` and re-run the sync.
    #[error("chain reorg detected at height {at_height}; wallet state must roll back")]
    ChainReorgDetected {
        /// Height at which the proposed-block parent hash diverged from the wallet's view.
        at_height: BlockHeight,
    },

    /// The transparent output script could not be mapped to a wallet-supported transparent
    /// address kind.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; the chain source returned a malformed or
    /// unsupported transparent output.
    #[error("transparent output {tx_id:?}:{output_index} has an unsupported script")]
    TransparentOutputNotRecognized {
        /// Transaction that produced the unsupported output.
        tx_id: zally_core::TxId,
        /// Output index within the producing transaction.
        output_index: u32,
    },

    /// A transparent output reported a value that cannot be represented as Zcash zatoshis.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; the chain source returned an invalid value.
    #[error("transparent output value {value_zat} exceeds the zatoshis range")]
    TransparentOutputValueOutOfRange {
        /// Invalid value in zatoshis.
        value_zat: u64,
    },

    /// A persisted row carried an integer that does not fit the typed field it projects
    /// onto (e.g. a `u32` block height stored as an `i64` that overflowed, or a `Zatoshis`
    /// amount above `MAX_MONEY`).
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]; corruption or a schema mismatch.
    #[error("persisted column '{column}' carried out-of-range raw {raw}")]
    RowValueOutOfRange {
        /// Column the offending value came from.
        column: &'static str,
        /// Stringified raw value for the operator.
        raw: String,
    },

    /// `propose_*` could not build a transaction because the wallet has too little
    /// spendable balance to cover amount + fee.
    ///
    /// Posture: [`FailurePosture::NotRetryable`] until balance is replenished.
    #[error(
        "insufficient spendable balance: required {required_zat:?}, available {available_zat:?}"
    )]
    InsufficientFunds {
        /// Total value (amount + fee) the proposal needed.
        required_zat: Zatoshis,
        /// Spendable value the wallet could draw on at proposal time.
        available_zat: Zatoshis,
    },

    /// The librustzcash proposal layer rejected the spend with a typed error that did not
    /// project onto one of the more specific variants above (e.g. balance change required
    /// for unsupported fee rule, missing anchor, address decode failure).
    ///
    /// Posture is carried explicitly so the wallet boundary does not have to infer it from
    /// the message string.
    #[error("proposal build failed ({posture:?}): {reason}")]
    ProposalBuildFailed {
        /// Underlying error description.
        reason: String,
        /// Operator-facing posture for this failure.
        posture: FailurePosture,
    },
}

impl StorageError {
    /// Operator-facing posture describing what the consumer may do.
    #[must_use]
    pub const fn posture(&self) -> FailurePosture {
        match self {
            Self::NotOpened
            | Self::AccountNotFound
            | Self::AccountAlreadyExists
            | Self::KeyDerivationFailed { .. }
            | Self::IdempotencyKeyConflict
            | Self::TransparentOutputNotRecognized { .. }
            | Self::TransparentOutputValueOutOfRange { .. }
            | Self::InsufficientFunds { .. } => FailurePosture::NotRetryable,
            Self::MigrationFailed { .. }
            | Self::ProverUnavailable
            | Self::RowValueOutOfRange { .. } => FailurePosture::RequiresOperator,
            Self::BlockingTaskFailed { .. } | Self::ChainReorgDetected { .. } => {
                FailurePosture::Retryable
            }
            Self::SqliteFailed { posture, .. } | Self::ProposalBuildFailed { posture, .. } => {
                *posture
            }
        }
    }

    /// Convenience: `true` when the same call may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.posture().allows_retry()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_error_posture_covers_every_variant() {
        let variants = [
            StorageError::NotOpened,
            StorageError::MigrationFailed { reason: "x".into() },
            StorageError::SqliteFailed {
                reason: "x".into(),
                posture: FailurePosture::Retryable,
            },
            StorageError::AccountNotFound,
            StorageError::AccountAlreadyExists,
            StorageError::BlockingTaskFailed { reason: "x".into() },
            StorageError::KeyDerivationFailed { reason: "x".into() },
            StorageError::ProverUnavailable,
            StorageError::IdempotencyKeyConflict,
            StorageError::ChainReorgDetected {
                at_height: BlockHeight::from(1),
            },
            StorageError::TransparentOutputNotRecognized {
                tx_id: zally_core::TxId::from_bytes([0_u8; 32]),
                output_index: 0,
            },
            StorageError::TransparentOutputValueOutOfRange {
                value_zat: u64::MAX,
            },
            StorageError::RowValueOutOfRange {
                column: "x",
                raw: "y".to_owned(),
            },
            StorageError::InsufficientFunds {
                required_zat: Zatoshis::zero(),
                available_zat: Zatoshis::zero(),
            },
            StorageError::ProposalBuildFailed {
                reason: "x".into(),
                posture: FailurePosture::NotRetryable,
            },
        ];
        for e in variants {
            let _ = e.posture();
            let _ = e.is_retryable();
        }
    }

    #[test]
    fn sqlite_failed_posture_drives_classification() {
        let retryable = StorageError::SqliteFailed {
            reason: "lock contention".into(),
            posture: FailurePosture::Retryable,
        };
        assert!(retryable.is_retryable());
        let permanent = StorageError::SqliteFailed {
            reason: "no such table".into(),
            posture: FailurePosture::RequiresOperator,
        };
        assert!(!permanent.is_retryable());
    }
}
