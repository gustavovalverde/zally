//! Zally chain-read and broadcast plane.
//!
//! Defines the [`ChainSource`] and [`Submitter`] trait surfaces consumed by
//! [`zally_wallet`](https://docs.rs/zally-wallet). A `ZinderChainSource` plus
//! `ZinderSubmitter` implementation is available behind the `zinder` cargo feature;
//! `zally_testkit::MockChainSource` and `MockSubmitter` cover unit-test wiring.

mod error;
mod source;
mod submitter;
mod transaction;
#[cfg(feature = "zinder")]
mod zinder_source;
#[cfg(feature = "zinder")]
mod zinder_submitter;

pub use error::{ChainSourceError, SubmitterError};
/// Re-export of [`zally_core::FailurePosture`] so chain consumers can keep importing the
/// posture type from `zally_chain` without taking a direct `zally_core` dependency.
pub use zally_core::FailurePosture;

pub use source::{
    BlockHeightRange, BlockId, ChainEpoch, ChainEpochCommitted, ChainEpochId, ChainEvent,
    ChainEventCursor, ChainEventCursorRecovery, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainEventStreamStart, ChainRangeReverted, ChainSource, CompactBlockStream, ShieldedPool,
    SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};
pub use submitter::{RejectionReason, SubmitOutcome, Submitter};
pub use transaction::{
    TransactionParseError, parse_transaction_expiry_height, parse_transaction_id,
};
pub use zally_core::{CompactBlockArtifact, TreeStateArtifact};
#[cfg(feature = "zinder")]
pub use zinder_source::{ZinderChainSource, ZinderRemoteOptions};
#[cfg(feature = "zinder")]
pub use zinder_submitter::ZinderSubmitter;
