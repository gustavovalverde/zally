//! Errors returned by [`WalletStorage`](crate::WalletStorage) operations.

/// Error returned by [`WalletStorage`](crate::WalletStorage) operations.
///
/// Canonical error type for every storage backend. Backends that own a different native error
/// translate it inside their impl rather than exposing an associated `Error` type on the trait.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// The caller invoked an operation before [`crate::WalletStorage::open_or_create`].
    ///
    /// `not_retryable`: call `open_or_create` first.
    #[error("wallet storage was not opened; call open_or_create first")]
    NotOpened,

    /// Schema migration failed.
    ///
    /// `requires_operator`: a schema mismatch needs manual intervention.
    #[error("wallet database migration failed: {reason}")]
    MigrationFailed {
        /// Underlying error description.
        reason: String,
    },

    /// A sqlite operation failed. The posture depends on the underlying cause: lock contention
    /// is retryable; a missing table or invalid schema is not.
    #[error("sqlite error: {reason}")]
    SqliteFailed {
        /// Underlying error description.
        reason: String,
        /// Whether retrying the same call can reasonably succeed.
        is_retryable: bool,
    },

    /// The requested account was not found.
    ///
    /// `not_retryable`.
    #[error("account not found in wallet")]
    AccountNotFound,

    /// An account already exists in this wallet.
    ///
    /// `not_retryable`: call [`crate::WalletStorage::find_account_for_seed`] or
    /// `Wallet::open` instead. Zally holds one account per wallet.
    #[error("an account already exists in this wallet; one account per wallet")]
    AccountAlreadyExists,

    /// A background task panicked or was cancelled.
    ///
    /// `retryable`: the tokio runtime may accept the task on retry.
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed {
        /// Underlying error description.
        reason: String,
    },

    /// Key derivation inside a storage operation failed.
    ///
    /// `not_retryable`: derivation is deterministic.
    #[error("key derivation failed: {reason}")]
    KeyDerivationFailed {
        /// Underlying error description.
        reason: String,
    },

    /// Sapling prover params are not available on disk.
    ///
    /// `requires_operator`: download the Sapling spend/output parameters into the
    /// platform-default location (`~/.local/share/ZcashParams/` on macOS;
    /// `~/.zcash-params/` on Linux). The upstream `zcash_proofs::download_sapling_parameters`
    /// helper or the canonical `zcash-params` distribution bucket are the sources.
    #[error(
        "Sapling prover parameters are not available; install sapling-spend.params and \
         sapling-output.params into the platform-default location"
    )]
    ProverUnavailable,

    /// The supplied `IdempotencyKey` already maps to a different `TxId` in the ledger.
    ///
    /// `not_retryable`: the wallet layer surfaces the prior `TxId` to the caller instead of
    /// overwriting the ledger entry.
    #[error(
        "idempotency key already bound to a different transaction; \
         prior tx_id was recorded for this key"
    )]
    IdempotencyKeyConflict,

    /// `scan_blocks` rejected the batch because the chain rolled back. The wallet's view at
    /// `at_height` does not match the parent hash of the next block the chain source served,
    /// so a reorg has occurred between the last successful scan and this attempt.
    ///
    /// `retryable`: callers truncate the wallet to before `at_height` and re-run the sync.
    #[error("chain reorg detected at height {at_height}; wallet state must roll back")]
    ChainReorgDetected {
        /// Height at which the proposed-block parent hash diverged from the wallet's view.
        at_height: zally_core::BlockHeight,
    },
}

impl StorageError {
    /// Whether the same call may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::SqliteFailed { is_retryable, .. } => *is_retryable,
            Self::BlockingTaskFailed { .. } | Self::ChainReorgDetected { .. } => true,
            Self::NotOpened
            | Self::MigrationFailed { .. }
            | Self::AccountNotFound
            | Self::AccountAlreadyExists
            | Self::KeyDerivationFailed { .. }
            | Self::ProverUnavailable
            | Self::IdempotencyKeyConflict => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_error_retryable_match_complete() {
        let variants: [StorageError; 10] = [
            StorageError::NotOpened,
            StorageError::MigrationFailed { reason: "x".into() },
            StorageError::SqliteFailed {
                reason: "x".into(),
                is_retryable: true,
            },
            StorageError::AccountNotFound,
            StorageError::AccountAlreadyExists,
            StorageError::BlockingTaskFailed { reason: "x".into() },
            StorageError::KeyDerivationFailed { reason: "x".into() },
            StorageError::ProverUnavailable,
            StorageError::IdempotencyKeyConflict,
            StorageError::ChainReorgDetected {
                at_height: zally_core::BlockHeight::from(1),
            },
        ];
        for e in variants {
            let _ = e.is_retryable();
        }
    }

    #[test]
    fn storage_error_sqlite_retryable_field_drives_method() {
        let retryable = StorageError::SqliteFailed {
            reason: "lock contention".into(),
            is_retryable: true,
        };
        assert!(retryable.is_retryable());
        let permanent = StorageError::SqliteFailed {
            reason: "no such table".into(),
            is_retryable: false,
        };
        assert!(!permanent.is_retryable());
    }
}
