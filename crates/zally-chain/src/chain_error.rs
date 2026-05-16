//! Errors for the chain-read and broadcast planes.
//!
//! Every variant maps onto exactly one [`FailurePosture`]; the posture is the
//! operator-facing contract that drives retry, circuit-breaker, and alerting
//! decisions at the wallet boundary. The variant tag describes *what
//! happened* (which kind of failure the source observed); the posture
//! describes *what the consumer may do about it*.
//!
//! See [`docs/adrs/0002-source-failure-posture.md`] for the architectural
//! contract that anchors this vocabulary, including the upstream zinder
//! `ADR-0013` it inherits.

use zally_core::{BlockHeight, Network};

#[cfg(feature = "zinder")]
use zinder_client::{IndexerError, RetryPolicy as IndexerRetryPolicy};

/// Operator-facing classification of a chain-source or submitter failure.
///
/// Three classes are sufficient for wallet-side lifecycle decisions:
/// transient backend trouble that benefits from retry, conditions that
/// require operator action before the request can succeed, and caller bugs
/// that retrying will not help. The three labels are the canonical
/// operator-facing names used in zally's error vocabulary and metrics.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum FailurePosture {
    /// Transient backend trouble. Retry with backoff is the appropriate
    /// response; the circuit breaker trips on consecutive failures.
    Retryable,
    /// An operator must intervene before the request can succeed
    /// (capability missing, configuration mismatch, upstream returning
    /// malformed bytes). Callers must surface this and stop retrying.
    RequiresOperator,
    /// The request itself is wrong or out of bounds. Callers fix the
    /// request and re-issue; retrying the same input fails again.
    NotRetryable,
}

impl FailurePosture {
    /// Stable kebab-case label for metrics, logs, and readiness payloads.
    ///
    /// Do not rename without coordinating dashboards: the label is the
    /// operator-facing identifier.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Retryable => "retryable",
            Self::RequiresOperator => "requires_operator",
            Self::NotRetryable => "not_retryable",
        }
    }

    /// `true` when the caller may issue the same request again.
    ///
    /// Equivalent to `matches!(self, Self::Retryable)`. Provided for
    /// callers that just need a boolean retry decision.
    #[must_use]
    pub const fn allows_retry(self) -> bool {
        matches!(self, Self::Retryable)
    }
}

#[cfg(feature = "zinder")]
impl From<IndexerRetryPolicy> for FailurePosture {
    #[allow(
        clippy::wildcard_enum_match_arm,
        clippy::match_same_arms,
        reason = "IndexerRetryPolicy is #[non_exhaustive]; the OperatorActionRequired arm \
                  documents the canonical mapping for the today-known variant, and the \
                  wildcard arm picks the same conservative posture for any future unknown \
                  variant so a new retry class never silently masquerades as Retryable."
    )]
    fn from(policy: IndexerRetryPolicy) -> Self {
        match policy {
            IndexerRetryPolicy::RetryWithBackoff => Self::Retryable,
            IndexerRetryPolicy::OperatorActionRequired => Self::RequiresOperator,
            IndexerRetryPolicy::ClientError => Self::NotRetryable,
            _ => Self::RequiresOperator,
        }
    }
}

/// Error returned by [`crate::ChainSource`] operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChainSourceError {
    /// The source is temporarily unavailable.
    ///
    /// Posture: [`FailurePosture::Retryable`]. Generic, source-agnostic
    /// signal used by mocks and any non-zinder `ChainSource` implementation
    /// to surface a transient backend stall.
    #[error("chain source temporarily unavailable: {reason}")]
    Unavailable {
        /// Underlying cause description.
        reason: String,
    },

    /// Requested height is below the source's earliest available block.
    ///
    /// Posture: [`FailurePosture::NotRetryable`].
    #[error(
        "block height {requested_height} is below source's earliest available height {earliest_height}"
    )]
    BlockHeightBelowFloor {
        /// Height the caller requested.
        requested_height: BlockHeight,
        /// Earliest height the source can serve.
        earliest_height: BlockHeight,
    },

    /// Requested height is above the source's current tip.
    ///
    /// Posture: [`FailurePosture::NotRetryable`] until the chain advances; caller
    /// should re-query `chain_tip()`.
    #[error("block height {requested_height} is above source's current tip {tip_height}")]
    BlockHeightAboveTip {
        /// Height the caller requested.
        requested_height: BlockHeight,
        /// Tip height the source currently exposes.
        tip_height: BlockHeight,
    },

    /// Configuration mismatch between the source and the caller.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`].
    #[error(
        "network mismatch: chain_source_network={chain_source_network:?}, requested_network={requested_network:?}"
    )]
    NetworkMismatch {
        /// Network the chain source was opened for.
        chain_source_network: Network,
        /// Network the caller is using.
        requested_network: Network,
    },

    /// A compact block returned by the source could not be parsed.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]. The malformed bytes are
    /// canonical for the source, so the operator must investigate the
    /// upstream version or storage corruption rather than the caller
    /// re-issuing the request.
    #[error("malformed compact block at height {block_height}: {reason}")]
    MalformedCompactBlock {
        /// Height of the offending block.
        block_height: BlockHeight,
        /// Underlying decode error description.
        reason: String,
    },

    /// A background task panicked or was cancelled.
    ///
    /// Posture: [`FailurePosture::Retryable`].
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed {
        /// Underlying error description.
        reason: String,
    },

    /// A zinder client call failed.
    ///
    /// Posture: derived from [`IndexerError::retry_policy`] via
    /// [`FailurePosture::from`]; preserves the typed `IndexerError`
    /// variant so operators see the canonical zinder cause without the
    /// adapter collapsing it into a generic string.
    #[cfg(feature = "zinder")]
    #[error("zinder indexer error: {0}")]
    Indexer(#[from] IndexerError),
}

impl ChainSourceError {
    /// Operator-facing posture describing what the consumer may do.
    #[must_use]
    pub fn posture(&self) -> FailurePosture {
        match self {
            Self::Unavailable { .. } | Self::BlockingTaskFailed { .. } => FailurePosture::Retryable,
            Self::BlockHeightBelowFloor { .. } | Self::BlockHeightAboveTip { .. } => {
                FailurePosture::NotRetryable
            }
            Self::NetworkMismatch { .. } | Self::MalformedCompactBlock { .. } => {
                FailurePosture::RequiresOperator
            }
            #[cfg(feature = "zinder")]
            Self::Indexer(err) => FailurePosture::from(err.retry_policy()),
        }
    }
}

/// Error returned by [`crate::Submitter`] operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubmitterError {
    /// The submitter is temporarily unavailable.
    ///
    /// Posture: [`FailurePosture::Retryable`]. Generic signal used by mocks
    /// and any non-zinder `Submitter` implementation.
    #[error("submitter temporarily unavailable: {reason}")]
    Unavailable {
        /// Underlying cause description.
        reason: String,
    },

    /// Configuration mismatch between the submitter and the transaction.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`].
    #[error("network mismatch: submitter={submitter:?}, transaction={transaction:?}")]
    NetworkMismatch {
        /// Network the submitter is bound to.
        submitter: Network,
        /// Network the transaction is for.
        transaction: Network,
    },

    /// A background task panicked or was cancelled.
    ///
    /// Posture: [`FailurePosture::Retryable`].
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed {
        /// Underlying error description.
        reason: String,
    },

    /// A zinder client call failed.
    ///
    /// Posture: derived from [`IndexerError::retry_policy`].
    #[cfg(feature = "zinder")]
    #[error("zinder indexer error: {0}")]
    Indexer(#[from] IndexerError),
}

impl SubmitterError {
    /// Operator-facing posture describing what the consumer may do.
    #[must_use]
    pub fn posture(&self) -> FailurePosture {
        match self {
            Self::Unavailable { .. } | Self::BlockingTaskFailed { .. } => FailurePosture::Retryable,
            Self::NetworkMismatch { .. } => FailurePosture::RequiresOperator,
            #[cfg(feature = "zinder")]
            Self::Indexer(err) => FailurePosture::from(err.retry_policy()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_posture_labels_are_stable() {
        assert_eq!(FailurePosture::Retryable.label(), "retryable");
        assert_eq!(
            FailurePosture::RequiresOperator.label(),
            "requires_operator"
        );
        assert_eq!(FailurePosture::NotRetryable.label(), "not_retryable");
    }

    #[test]
    fn allows_retry_only_for_retryable_posture() {
        assert!(FailurePosture::Retryable.allows_retry());
        assert!(!FailurePosture::RequiresOperator.allows_retry());
        assert!(!FailurePosture::NotRetryable.allows_retry());
    }

    #[test]
    fn chain_source_error_postures_cover_every_variant() {
        let cases: &[(ChainSourceError, FailurePosture)] = &[
            (
                ChainSourceError::Unavailable { reason: "x".into() },
                FailurePosture::Retryable,
            ),
            (
                ChainSourceError::BlockHeightBelowFloor {
                    requested_height: BlockHeight::from(0),
                    earliest_height: BlockHeight::from(1),
                },
                FailurePosture::NotRetryable,
            ),
            (
                ChainSourceError::BlockHeightAboveTip {
                    requested_height: BlockHeight::from(2),
                    tip_height: BlockHeight::from(1),
                },
                FailurePosture::NotRetryable,
            ),
            (
                ChainSourceError::NetworkMismatch {
                    chain_source_network: Network::Mainnet,
                    requested_network: Network::Testnet,
                },
                FailurePosture::RequiresOperator,
            ),
            (
                ChainSourceError::MalformedCompactBlock {
                    block_height: BlockHeight::from(1),
                    reason: "x".into(),
                },
                FailurePosture::RequiresOperator,
            ),
            (
                ChainSourceError::BlockingTaskFailed { reason: "x".into() },
                FailurePosture::Retryable,
            ),
        ];
        for (variant, expected) in cases {
            assert_eq!(variant.posture(), *expected, "variant {variant}");
        }
    }

    #[test]
    fn submitter_error_postures_cover_every_variant() {
        let cases: &[(SubmitterError, FailurePosture)] = &[
            (
                SubmitterError::Unavailable { reason: "x".into() },
                FailurePosture::Retryable,
            ),
            (
                SubmitterError::NetworkMismatch {
                    submitter: Network::Mainnet,
                    transaction: Network::Testnet,
                },
                FailurePosture::RequiresOperator,
            ),
            (
                SubmitterError::BlockingTaskFailed { reason: "x".into() },
                FailurePosture::Retryable,
            ),
        ];
        for (variant, expected) in cases {
            assert_eq!(variant.posture(), *expected, "variant {variant}");
        }
    }

    #[cfg(feature = "zinder")]
    #[test]
    fn indexer_retry_policy_maps_to_posture() {
        assert_eq!(
            FailurePosture::from(IndexerRetryPolicy::RetryWithBackoff),
            FailurePosture::Retryable,
        );
        assert_eq!(
            FailurePosture::from(IndexerRetryPolicy::OperatorActionRequired),
            FailurePosture::RequiresOperator,
        );
        assert_eq!(
            FailurePosture::from(IndexerRetryPolicy::ClientError),
            FailurePosture::NotRetryable,
        );
    }

    #[cfg(feature = "zinder")]
    #[test]
    fn chain_source_indexer_variant_delegates_posture_to_retry_policy() {
        let err = ChainSourceError::Indexer(IndexerError::InvalidRequest {
            reason: "bad".into(),
        });
        assert_eq!(err.posture(), FailurePosture::NotRetryable);
        let err = ChainSourceError::Indexer(IndexerError::ServiceUnavailable {
            reason: "down".into(),
        });
        assert_eq!(err.posture(), FailurePosture::Retryable);
        let err = ChainSourceError::Indexer(IndexerError::FailedPrecondition {
            reason: "schema".into(),
        });
        assert_eq!(err.posture(), FailurePosture::RequiresOperator);
    }
}
