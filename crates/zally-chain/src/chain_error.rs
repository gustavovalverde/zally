//! Errors for the chain-read and broadcast planes.

use zally_core::{BlockHeight, Network};

/// Error returned by [`crate::ChainSource`] operations.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChainSourceError {
    /// The source is temporarily unavailable.
    ///
    /// `retryable`: transient network failure or upstream rebuild.
    #[error("chain source temporarily unavailable: {reason}")]
    Unavailable {
        /// Underlying cause description.
        reason: String,
    },

    /// Requested height is below the source's earliest available block.
    ///
    /// `not_retryable`.
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
    /// `not_retryable` until the chain advances; caller should re-query `chain_tip()`.
    #[error("block height {requested_height} is above source's current tip {tip_height}")]
    BlockHeightAboveTip {
        /// Height the caller requested.
        requested_height: BlockHeight,
        /// Tip height the source currently exposes.
        tip_height: BlockHeight,
    },

    /// Configuration mismatch between the source and the caller.
    ///
    /// `requires_operator`.
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
    /// `not_retryable`: the malformed block is canonical for the source.
    #[error("malformed compact block at height {block_height}: {reason}")]
    MalformedCompactBlock {
        /// Height of the offending block.
        block_height: BlockHeight,
        /// Underlying decode error description.
        reason: String,
    },

    /// A background task panicked or was cancelled.
    ///
    /// `retryable`.
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed {
        /// Underlying error description.
        reason: String,
    },

    /// Posture varies per upstream cause. Lock contention is retryable; permanent failures are not.
    #[error("upstream chain-source error: {reason}")]
    UpstreamFailed {
        /// Underlying cause description.
        reason: String,
        /// Whether retrying the same call can reasonably succeed.
        is_retryable: bool,
    },
}

impl ChainSourceError {
    /// Whether the same call may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Unavailable { .. } | Self::BlockingTaskFailed { .. } => true,
            Self::UpstreamFailed { is_retryable, .. } => *is_retryable,
            Self::BlockHeightBelowFloor { .. }
            | Self::BlockHeightAboveTip { .. }
            | Self::NetworkMismatch { .. }
            | Self::MalformedCompactBlock { .. } => false,
        }
    }
}

/// Error returned by [`crate::Submitter`] operations.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubmitterError {
    /// The submitter is temporarily unavailable.
    ///
    /// `retryable`.
    #[error("submitter temporarily unavailable: {reason}")]
    Unavailable {
        /// Underlying cause description.
        reason: String,
    },

    /// Configuration mismatch between the submitter and the transaction.
    ///
    /// `requires_operator`.
    #[error("network mismatch: submitter={submitter:?}, transaction={transaction:?}")]
    NetworkMismatch {
        /// Network the submitter is bound to.
        submitter: Network,
        /// Network the transaction is for.
        transaction: Network,
    },

    /// A background task panicked or was cancelled.
    ///
    /// `retryable`.
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed {
        /// Underlying error description.
        reason: String,
    },

    /// Posture varies per upstream cause.
    #[error("upstream submitter error: {reason}")]
    UpstreamFailed {
        /// Underlying cause description.
        reason: String,
        /// Whether retrying the same call can reasonably succeed.
        is_retryable: bool,
    },
}

impl SubmitterError {
    /// Whether the same call may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Unavailable { .. } | Self::BlockingTaskFailed { .. } => true,
            Self::UpstreamFailed { is_retryable, .. } => *is_retryable,
            Self::NetworkMismatch { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_source_error_retryable_match_complete() {
        let variants = [
            ChainSourceError::Unavailable { reason: "x".into() },
            ChainSourceError::BlockHeightBelowFloor {
                requested_height: BlockHeight::from(0),
                earliest_height: BlockHeight::from(1),
            },
            ChainSourceError::BlockHeightAboveTip {
                requested_height: BlockHeight::from(2),
                tip_height: BlockHeight::from(1),
            },
            ChainSourceError::NetworkMismatch {
                chain_source_network: Network::Mainnet,
                requested_network: Network::Testnet,
            },
            ChainSourceError::MalformedCompactBlock {
                block_height: BlockHeight::from(1),
                reason: "x".into(),
            },
            ChainSourceError::BlockingTaskFailed { reason: "x".into() },
            ChainSourceError::UpstreamFailed {
                reason: "x".into(),
                is_retryable: true,
            },
        ];
        for e in variants {
            let _ = e.is_retryable();
        }
    }

    #[test]
    fn submitter_error_retryable_match_complete() {
        let variants = [
            SubmitterError::Unavailable { reason: "x".into() },
            SubmitterError::NetworkMismatch {
                submitter: Network::Mainnet,
                transaction: Network::Testnet,
            },
            SubmitterError::BlockingTaskFailed { reason: "x".into() },
            SubmitterError::UpstreamFailed {
                reason: "x".into(),
                is_retryable: false,
            },
        ];
        for e in variants {
            let _ = e.is_retryable();
        }
    }
}
