//! `ChainSource` trait and the supporting domain vocabulary.

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use zally_core::{BlockHeight, Network, TxId};

use crate::chain_error::ChainSourceError;

/// Inclusive block-height range.
///
/// Constructors validate `start_height <= end_height` so iteration order is unambiguous.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockHeightRange {
    /// First block in the range (inclusive).
    pub start_height: BlockHeight,
    /// Last block in the range (inclusive).
    pub end_height: BlockHeight,
}

impl BlockHeightRange {
    /// Constructs a range. Returns `None` if `start_height > end_height`.
    #[must_use]
    pub fn new(start_height: BlockHeight, end_height: BlockHeight) -> Option<Self> {
        if start_height.as_u32() > end_height.as_u32() {
            None
        } else {
            Some(Self {
                start_height,
                end_height,
            })
        }
    }

    /// Number of blocks in the range (inclusive on both ends).
    #[must_use]
    pub const fn block_count(&self) -> u32 {
        self.end_height.as_u32() - self.start_height.as_u32() + 1
    }
}

/// Shielded pool selector. Zally's vocabulary for `zcash_protocol::ShieldedProtocol`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ShieldedPool {
    /// Sapling pool.
    Sapling,
    /// Orchard pool.
    Orchard,
}

/// Index of a subtree root in a pool's note commitment tree.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SubtreeIndex(pub u32);

/// A subtree root for a shielded pool's note commitment tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SubtreeRoot {
    /// Index of this subtree root in the pool's tree.
    pub index: SubtreeIndex,
    /// 32-byte digest of the subtree's root.
    pub root_bytes: [u8; 32],
}

/// A spendable transparent UTXO at the source's current tip.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransparentUtxo {
    /// Transaction that produced this output.
    pub tx_id: TxId,
    /// Output index within the producing transaction.
    pub output_index: u32,
    /// Value in zatoshis.
    pub value_zat: u64,
    /// Block height at which the output was mined.
    pub confirmed_at_height: BlockHeight,
    /// Output `scriptPubKey` bytes.
    pub script_pub_key: Vec<u8>,
}

/// Status of a transaction at the source's current view.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum TransactionStatus {
    /// Transaction is mined at `confirmed_at_height`.
    Confirmed {
        /// Transaction identifier.
        tx_id: TxId,
        /// Height at which the transaction was mined.
        confirmed_at_height: BlockHeight,
    },
    /// Transaction is in the mempool but not yet mined.
    InMempool {
        /// Transaction identifier.
        tx_id: TxId,
    },
    /// Transaction is unknown to the source.
    NotFound,
}

/// Chain-event variant the source emits.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ChainEvent {
    /// Chain tip advanced (no reorg).
    ChainTipAdvanced {
        /// Block range that was finalized.
        committed_range: BlockHeightRange,
        /// New tip height.
        new_tip_height: BlockHeight,
    },
    /// Reorg detected: reverted blocks were replaced.
    ChainReorged {
        /// Block range that was reverted.
        reverted_range: BlockHeightRange,
        /// Block range that was committed in its place.
        committed_range: BlockHeightRange,
        /// New tip height.
        new_tip_height: BlockHeight,
    },
}

/// Stream of compact blocks. Each item is exactly one block.
pub type CompactBlockStream = Pin<
    Box<
        dyn Stream<
                Item = Result<
                    zcash_client_backend::proto::compact_formats::CompactBlock,
                    ChainSourceError,
                >,
            > + Send,
    >,
>;

/// Stream of chain events.
pub type ChainEventStream =
    Pin<Box<dyn Stream<Item = Result<ChainEvent, ChainSourceError>> + Send>>;

/// Chain-read plane.
///
/// Testkit consumers use `zally_testkit::MockChainSource`. The default `ZinderChainSource`
/// implementation lands when Zinder's workspace stops pulling yanked transitive
/// dependencies. Implementations route blocking work through `tokio::task::spawn_blocking`
/// and emit Zally-vocabulary errors.
#[async_trait]
pub trait ChainSource: Send + Sync + 'static {
    /// Network this chain source is bound to.
    fn network(&self) -> Network;

    /// Current visible chain tip height.
    async fn chain_tip(&self) -> Result<BlockHeight, ChainSourceError>;

    /// Streams compact blocks in `block_range` (inclusive on both ends).
    async fn compact_blocks(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError>;

    /// Returns the canonical tree state at `block_height`.
    async fn tree_state_at(
        &self,
        block_height: BlockHeight,
    ) -> Result<zcash_client_backend::proto::service::TreeState, ChainSourceError>;

    /// Returns subtree roots for `pool` starting at `start_index`.
    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError>;

    /// Looks up a transaction.
    async fn transaction_status(&self, tx_id: TxId) -> Result<TransactionStatus, ChainSourceError>;

    /// Returns confirmed UTXOs for a transparent address at the source's current tip.
    ///
    /// Slice 2 takes the address as raw `scriptPubKey` bytes; Slice 3 introduces a typed
    /// `TransparentAddress` when the spend flow needs it.
    async fn transparent_utxos(
        &self,
        script_pub_key: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError>;

    /// Subscribes to chain events.
    async fn chain_events(&self) -> Result<ChainEventStream, ChainSourceError>;
}
