//! Zally chain-read and broadcast plane.
//!
//! Defines the [`ChainSource`] and [`Submitter`] trait surfaces consumed by
//! [`zally_wallet`](https://docs.rs/zally-wallet). A `ZinderChainSource` plus
//! `ZinderSubmitter` implementation is available behind the `zinder` cargo feature;
//! `zally_testkit::MockChainSource` and `MockSubmitter` cover unit-test wiring.

mod buffered_source;
mod error;
mod source;
mod submitter;
#[cfg(feature = "zinder")]
mod zinder_source;
#[cfg(feature = "zinder")]
mod zinder_submitter;

pub use buffered_source::{BufferedChainSource, BufferedChainSourceError};
pub use error::{ChainSourceError, SubmitterError};
/// Re-export of [`zally_core::FailurePosture`] so chain consumers can keep importing the
/// posture type from `zally_chain` without taking a direct `zally_core` dependency.
pub use zally_core::FailurePosture;

pub use source::{
    BlockHeightRange, ChainEvent, ChainEventCursor, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainEventStream, ChainEventStreamStart, ChainSource, CompactBlockStream, ShieldedPool,
    SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};
pub use submitter::{RejectionReason, SubmitOutcome, Submitter};
/// Re-export of `zcash_client_backend::data_api::chain::ChainState` so callers of the Zally
/// scan API do not need to depend on `zcash_client_backend` directly.
pub use zcash_client_backend::data_api::chain::ChainState;
#[cfg(feature = "zinder")]
pub use zinder_source::{ZinderChainSource, ZinderRemoteOptions};
#[cfg(feature = "zinder")]
pub use zinder_submitter::ZinderSubmitter;
