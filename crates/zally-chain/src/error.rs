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

use zally_core::{BlockHeight, FailurePosture, Network};

#[cfg(feature = "zinder")]
use zinder_client::{IndexerError, RetryPolicy as IndexerRetryPolicy};

/// Maps a zinder `IndexerRetryPolicy` onto a [`FailurePosture`].
///
/// Defined here rather than on `FailurePosture` so `zally-core` does not learn about the
/// zinder client. The mapping picks `RequiresOperator` for any future unknown variant so a
/// new retry class never silently masquerades as `Retryable`.
#[cfg(feature = "zinder")]
pub(crate) fn posture_for_indexer(policy: IndexerRetryPolicy) -> FailurePosture {
    #[allow(
        clippy::wildcard_enum_match_arm,
        clippy::match_same_arms,
        reason = "IndexerRetryPolicy is #[non_exhaustive]; the wildcard arm picks the same \
                  conservative posture as OperatorActionRequired for any future variant"
    )]
    match policy {
        IndexerRetryPolicy::RetryWithBackoff | IndexerRetryPolicy::RefreshChainEpoch => {
            FailurePosture::Retryable
        }
        IndexerRetryPolicy::OperatorActionRequired => FailurePosture::RequiresOperator,
        IndexerRetryPolicy::ClientError => FailurePosture::NotRetryable,
        _ => FailurePosture::RequiresOperator,
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

    /// Requested height is above the source's visible chain tip.
    ///
    /// Posture: [`FailurePosture::NotRetryable`] until the chain advances; caller
    /// should acquire a new [`crate::ChainEpoch`].
    #[error("block height {requested_height} is above source's visible tip {visible_tip_height}")]
    BlockHeightAboveVisibleTip {
        /// Height the caller requested.
        requested_height: BlockHeight,
        /// Visible chain tip height the source currently exposes.
        visible_tip_height: BlockHeight,
    },

    /// The connected source does not advertise capabilities required by this operation.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]. The endpoint must be upgraded or
    /// reconfigured before the operation can be retried.
    #[error("chain source does not advertise required capabilities: {capabilities:?}")]
    CapabilitiesUnavailable {
        /// Stable Zinder capability strings absent from `ServerInfo`.
        capabilities: Vec<String>,
    },

    /// The source contract revision is older than this Zally build requires.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]. Upgrade the endpoint before retrying.
    #[error(
        "chain source contract revision {actual_revision} is older than required revision {minimum_revision}"
    )]
    ContractRevisionUnsupported {
        /// Minimum revision implemented by this Zally build.
        minimum_revision: u32,
        /// Revision advertised by the source.
        actual_revision: u32,
    },

    /// The exact chain epoch pinned for the current sync attempt is no longer retained.
    ///
    /// Posture: [`FailurePosture::Retryable`]. Restarting sync obtains a fresh visible epoch
    /// before requesting any settled artifacts.
    #[error("chain epoch pin became unavailable; restart sync against a fresh epoch")]
    ChainEpochPinUnavailable,

    /// The resume cursor precedes the source's retained chain-event window.
    ///
    /// Posture: [`FailurePosture::Retryable`]. Apply `recovery` after reconciling the
    /// source epoch and wallet state.
    #[error("chain event cursor expired; restart from the earliest retained event")]
    ChainEventCursorExpired {
        /// Safe source-neutral subscription recovery.
        recovery: crate::ChainEventCursorRecovery,
    },

    /// The source returned a protocol variant this Zally build does not understand.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]. Upgrade Zally before interpreting the
    /// new response; silently fabricating an older meaning would corrupt wallet state.
    #[error("unsupported chain-source response variant: {response}")]
    UnsupportedResponse {
        /// Stable name of the response family containing the unknown variant.
        response: &'static str,
    },

    /// The chain source returned a tree-state anchor at a height other than
    /// the one requested. The chain-source contract requires
    /// `tree_state_at(h)` to land at exactly `h` (or a height within the
    /// wallet's rewind cap that the wallet can realign to). This variant
    /// fires when a custom chain source violates the cadence contract from
    /// ADR-0005 and the wallet cannot recover.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]. The operator must
    /// investigate the chain source: re-querying yields the same answer.
    #[error(
        "chain source returned tree-state anchor at height {returned_height} for requested height {requested_height}; the chain source is violating the exact-height contract"
    )]
    TreeStateAnchorHeightMismatch {
        /// Height the wallet requested.
        requested_height: BlockHeight,
        /// Height the chain source actually returned.
        returned_height: BlockHeight,
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

    /// The compact-block stream did not contain the exact requested height sequence.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]. A successful range response must be
    /// complete, ordered, and contain each requested height exactly once.
    #[error(
        "compact-block stream expected height {expected_height}, but received {actual_height:?}"
    )]
    CompactBlockStreamOrder {
        /// Next height required by the inclusive request range.
        expected_height: BlockHeight,
        /// Height received instead, or `None` when the stream ended early.
        actual_height: Option<BlockHeight>,
    },

    /// A subtree-root page violated its requested bounds or ordering.
    #[error("malformed {pool:?} subtree-root page: {reason}")]
    MalformedSubtreeRootPage {
        /// Shielded pool whose page was malformed.
        pool: crate::ShieldedPool,
        /// Contract violation.
        reason: String,
    },

    /// A transparent UTXO response violated the pinned epoch or set contract.
    #[error("malformed transparent UTXO set: {reason}")]
    MalformedTransparentUtxoSet {
        /// Contract violation.
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

    /// The requested shielded pool has no subtree-root query path on this chain source.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]; upgrading to a chain-source backend
    /// that supports the pool is the only way to query subtree roots for it.
    #[error("shielded pool {pool:?} has no subtree-root query path on this chain source")]
    ShieldedPoolUnsupported {
        /// The pool the caller requested.
        pool: crate::source::ShieldedPool,
    },

    /// A zinder client call failed.
    ///
    /// Posture: derived from `IndexerError::retry_policy` via the crate-private
    /// `posture_for_indexer` adapter; preserves the typed `IndexerError` variant so operators
    /// see the canonical zinder cause without the adapter collapsing it into a generic string.
    #[cfg(feature = "zinder")]
    #[error("zinder indexer error: {0}")]
    Indexer(#[from] IndexerError),
}

impl ChainSourceError {
    /// Operator-facing posture describing what the consumer may do.
    #[must_use]
    pub fn posture(&self) -> FailurePosture {
        match self {
            Self::Unavailable { .. }
            | Self::BlockingTaskFailed { .. }
            | Self::ChainEpochPinUnavailable
            | Self::ChainEventCursorExpired { .. } => FailurePosture::Retryable,
            Self::BlockHeightBelowFloor { .. } | Self::BlockHeightAboveVisibleTip { .. } => {
                FailurePosture::NotRetryable
            }
            Self::NetworkMismatch { .. }
            | Self::CapabilitiesUnavailable { .. }
            | Self::ContractRevisionUnsupported { .. }
            | Self::UnsupportedResponse { .. }
            | Self::MalformedCompactBlock { .. }
            | Self::CompactBlockStreamOrder { .. }
            | Self::MalformedSubtreeRootPage { .. }
            | Self::MalformedTransparentUtxoSet { .. }
            | Self::TreeStateAnchorHeightMismatch { .. }
            | Self::ShieldedPoolUnsupported { .. } => FailurePosture::RequiresOperator,
            #[cfg(feature = "zinder")]
            Self::Indexer(err) => posture_for_indexer(err.retry_policy()),
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

    /// The submitter rejected the request before transaction broadcast.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]. Retrying the same bytes and request
    /// metadata cannot satisfy the submitter's contract.
    #[error("submitter rejected the request: {reason}")]
    RequestRejected {
        /// Stable rejection detail returned by the submitter.
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

    /// The connected source does not advertise transaction broadcast support.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]. The endpoint must be reconfigured
    /// with a broadcaster before submission can succeed.
    #[error("submitter source does not advertise required capabilities: {capabilities:?}")]
    CapabilitiesUnavailable {
        /// Stable Zinder capability strings absent from `ServerInfo`.
        capabilities: Vec<String>,
    },

    /// The source returned a broadcast outcome this Zally build does not understand.
    #[error("unsupported submitter response variant: {response}")]
    UnsupportedResponse {
        /// Stable name of the response family containing the unknown variant.
        response: &'static str,
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
            Self::RequestRejected { .. } => FailurePosture::NotRetryable,
            Self::NetworkMismatch { .. } => FailurePosture::RequiresOperator,
            Self::CapabilitiesUnavailable { .. } | Self::UnsupportedResponse { .. } => {
                FailurePosture::RequiresOperator
            }
            #[cfg(feature = "zinder")]
            Self::Indexer(err) => posture_for_indexer(err.retry_policy()),
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
                ChainSourceError::BlockHeightAboveVisibleTip {
                    requested_height: BlockHeight::from(2),
                    visible_tip_height: BlockHeight::from(1),
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
                ChainSourceError::UnsupportedResponse {
                    response: "ChainEvent",
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
                ChainSourceError::CompactBlockStreamOrder {
                    expected_height: BlockHeight::from(1),
                    actual_height: None,
                },
                FailurePosture::RequiresOperator,
            ),
            (
                ChainSourceError::TreeStateAnchorHeightMismatch {
                    requested_height: BlockHeight::from(2),
                    returned_height: BlockHeight::from(0),
                },
                FailurePosture::RequiresOperator,
            ),
            (
                ChainSourceError::BlockingTaskFailed { reason: "x".into() },
                FailurePosture::Retryable,
            ),
            (
                ChainSourceError::ShieldedPoolUnsupported {
                    pool: crate::source::ShieldedPool::Ironwood,
                },
                FailurePosture::RequiresOperator,
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
                SubmitterError::RequestRejected { reason: "x".into() },
                FailurePosture::NotRetryable,
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
            (
                SubmitterError::UnsupportedResponse {
                    response: "TransactionBroadcastOutcome",
                },
                FailurePosture::RequiresOperator,
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
            posture_for_indexer(IndexerRetryPolicy::RetryWithBackoff),
            FailurePosture::Retryable,
        );
        assert_eq!(
            posture_for_indexer(IndexerRetryPolicy::RefreshChainEpoch),
            FailurePosture::Retryable,
        );
        assert_eq!(
            posture_for_indexer(IndexerRetryPolicy::OperatorActionRequired),
            FailurePosture::RequiresOperator,
        );
        assert_eq!(
            posture_for_indexer(IndexerRetryPolicy::ClientError),
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
