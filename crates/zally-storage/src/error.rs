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

    /// The requested shielded pool has no commitment-tree write path in the pinned
    /// `zcash_client_backend` release.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]; upgrading to a release that adds the
    /// write path is the only way to record subtree roots for this pool.
    #[error("shielded pool {pool:?} has no subtree-root write path in this release")]
    ShieldedPoolUnsupported {
        /// The pool the caller requested.
        pool: zcash_protocol::ShieldedPool,
    },

    /// The supplied `IdempotencyKey` already maps to a different `TxId` in the ledger.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; the wallet layer surfaces the prior `TxId`
    /// to the caller instead of overwriting the ledger entry.
    #[error(
        "idempotency key already bound to a different transaction; \
         prior tx_id was recorded for this key"
    )]
    IdempotencyKeyConflict,

    /// `scan_blocks` rejected the batch because the chain source served a block whose parent
    /// hash does not match the wallet's stored view at `at_height`.
    ///
    /// Posture: [`FailurePosture::Retryable`]. The wallet scans to the live chain tip, so a
    /// divergence at `at_height` is an ordinary reorg of the head. The caller catches this and
    /// rewinds precisely below the orphan via `WalletStorage::truncate_to_chain_state`, then
    /// re-runs sync. The librustzcash rewind cap (100 blocks = `COINBASE_MATURITY`) bounds how
    /// far the truncate can go; if the divergence is deeper than the cap, the truncate call
    /// surfaces a separate `NotRetryable` storage failure and the operator must reset the
    /// wallet.
    #[error(
        "chain reorg detected at height {at_height}; wallet must roll back to {} and re-sync",
        at_height.as_u32().saturating_sub(1)
    )]
    ChainReorgDetected {
        /// Height at which the proposed-block parent hash diverged from the wallet's view.
        at_height: BlockHeight,
    },

    /// The scanner computed a note-commitment subtree root that disagrees with a root
    /// already stored in the wallet tree: the wallet's derived chain state no longer
    /// matches the chain, and re-issuing the same scan fails deterministically until the
    /// stale tree state is discarded.
    ///
    /// Posture: [`FailurePosture::NotRetryable`] for the issuing call; the sync driver
    /// treats this as its cue to rewind deeper or rebuild derived state from the birthday.
    #[error("note commitment tree conflict: {reason}")]
    CommitmentTreeConflict {
        /// Underlying error description.
        reason: String,
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

    /// `release_hold` or `finalize_hold` was called for an
    /// identifier the storage layer has no row for.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; the caller already released, finalized,
    /// or never created this reservation.
    #[error("dispense reservation not found")]
    HoldNotFound,

    /// A `create_hold` call supplied a `request_id` already bound to another
    /// active reservation.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; the wallet boundary should look up the
    /// existing reservation by request id and surface it idempotently.
    #[error("dispense reservation request id is already bound to a prior reservation")]
    HoldRequestConflict,
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
            | Self::CommitmentTreeConflict { .. }
            | Self::TransparentOutputNotRecognized { .. }
            | Self::TransparentOutputValueOutOfRange { .. }
            | Self::InsufficientFunds { .. }
            | Self::HoldNotFound
            | Self::HoldRequestConflict => FailurePosture::NotRetryable,
            Self::MigrationFailed { .. }
            | Self::ProverUnavailable
            | Self::ShieldedPoolUnsupported { .. }
            | Self::RowValueOutOfRange { .. } => FailurePosture::RequiresOperator,
            Self::ChainReorgDetected { .. } | Self::BlockingTaskFailed { .. } => {
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
            StorageError::CommitmentTreeConflict { reason: "x".into() },
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
            StorageError::HoldNotFound,
            StorageError::HoldRequestConflict,
            StorageError::ShieldedPoolUnsupported {
                pool: zcash_protocol::ShieldedPool::Ironwood,
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
