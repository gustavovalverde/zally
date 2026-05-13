//! Zally chain-read and broadcast plane.
//!
//! Defines the [`ChainSource`] and [`Submitter`] trait surfaces consumed by
//! [`zally_wallet`](https://docs.rs/zally-wallet). Slice 2 ships the trait surface plus the
//! `zally_testkit::MockChainSource` fixture; the default `ZinderChainSource` implementation
//! lands in a later slice once Zinder's workspace stops pulling yanked transitive
//! dependencies. `LightwalletdChainSource` lands with the same later slice.
//!
//! See RFC-0002 for design rationale.

mod buffered_block_source;
mod chain_error;
mod chain_source;
mod submitter;
#[cfg(feature = "zinder")]
mod zinder_chain_source;
#[cfg(feature = "zinder")]
mod zinder_submitter;

pub use buffered_block_source::{BufferedBlockSource, BufferedBlockSourceError};
pub use chain_error::{ChainSourceError, SubmitterError};

pub use chain_source::{
    BlockHeightRange, ChainEvent, ChainEventStream, ChainSource, CompactBlockStream, ShieldedPool,
    SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};
pub use submitter::{SubmitOutcome, Submitter};
/// Re-export of `zcash_client_backend::data_api::chain::ChainState` so callers of the Zally
/// scan API do not need to depend on `zcash_client_backend` directly.
pub use zcash_client_backend::data_api::chain::ChainState;
#[cfg(feature = "zinder")]
pub use zinder_chain_source::{ZinderChainSource, ZinderRemoteOptions};
#[cfg(feature = "zinder")]
pub use zinder_submitter::ZinderSubmitter;
