//! `Submitter` trait for transaction broadcast.

use async_trait::async_trait;
use zally_core::{Network, TxId};

use crate::error::SubmitterError;

/// Typed broadcast rejection reason.
///
/// Mirrors `zinder_core::BroadcastRejectionReason` so callers (notably
/// `zally-wallet`, `zally-testkit`, and their consumers) can dispatch on the
/// typed value without requiring the `zinder` cargo feature.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum RejectionReason {
    /// Node returned a rejection that did not match any known reason.
    #[default]
    Unknown,
    /// Submitted bytes were not a valid serialized transaction.
    InvalidEncoding,
    /// Verifier rejected one or more transaction signatures.
    InvalidSignature,
    /// Transaction's `nExpiryHeight` is at or below the visible tip.
    BadExpiryHeight,
    /// Transaction's consensus branch id does not match the network upgrade.
    BadConsensusBranch,
    /// Mempool is at capacity and refused the transaction.
    MempoolFull,
}

#[cfg(feature = "zinder")]
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive zinder_core::BroadcastRejectionReason maps unknown variants to Unknown"
)]
impl From<zinder_core::BroadcastRejectionReason> for RejectionReason {
    fn from(reason: zinder_core::BroadcastRejectionReason) -> Self {
        match reason {
            zinder_core::BroadcastRejectionReason::InvalidSignature => Self::InvalidSignature,
            zinder_core::BroadcastRejectionReason::BadExpiryHeight => Self::BadExpiryHeight,
            zinder_core::BroadcastRejectionReason::BadConsensusBranch => Self::BadConsensusBranch,
            zinder_core::BroadcastRejectionReason::MempoolFull => Self::MempoolFull,
            _ => Self::Unknown,
        }
    }
}

/// Outcome of a transaction broadcast.
///
/// Not `serde::Serialize` even with the `serde` feature, because
/// `RejectionReason` is `#[non_exhaustive]` and does not derive serde. The
/// outcome is an in-process value passed between the chain and wallet
/// planes; persistence layers serialize the resulting `WalletError` variant
/// (which carries `Debug` formatting of the typed reason) rather than the
/// raw outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SubmitOutcome {
    /// The mempool accepted the transaction.
    Accepted {
        /// Resulting transaction identifier.
        tx_id: TxId,
    },
    /// The mempool already had this transaction.
    Duplicate {
        /// Resulting transaction identifier.
        tx_id: TxId,
    },
    /// The upstream node accepted the broadcast into its download or
    /// verification queue but has not yet produced a final verdict.
    ///
    /// Re-submitting the same byte-identical transaction while the prior
    /// submission is in flight produces this state on Zebra. Callers should
    /// treat it as a success-equivalent for idempotency purposes: the
    /// pending-broadcast tracking row stays in place, the dispense path
    /// returns the tx id (so auto-shield and dispense pipelines record the
    /// same identifier that will eventually mine), and no retry is
    /// scheduled.
    ///
    /// `tx_id` echoes the caller-computed identifier rather than one
    /// returned by the upstream node, because Zebra does not echo a tx id
    /// in its `MempoolError::AlreadyQueued` response.
    Queued {
        /// Caller-computed transaction identifier of the queued broadcast.
        tx_id: TxId,
    },
    /// The transaction was rejected; retrying the same bytes will not succeed.
    Rejected {
        /// Typed rejection reason from the upstream node.
        reason: RejectionReason,
        /// Operator-facing message describing the rejection.
        detail: String,
    },
}

/// Transaction broadcast plane.
///
/// Implementations forward `submit` to whatever broadcast endpoint the operator runs.
/// Network mismatch fails closed at construction or at call time.
#[async_trait]
pub trait Submitter: Send + Sync + 'static {
    /// Network this submitter is bound to.
    fn network(&self) -> Network;

    /// Submits `raw_tx`. The outcome discriminates duplicate / rejected / accepted.
    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError>;
}
