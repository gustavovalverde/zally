//! `ChainSource` trait and the supporting domain vocabulary.

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use zally_core::{
    BlockHash, BlockHeight, CompactBlockArtifact, Network, TreeStateArtifact, TxId, Zatoshis,
};

use crate::error::ChainSourceError;

/// Inclusive block-height range.
///
/// Constructors validate `start_height <= end_height` so iteration order is unambiguous.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockHeightRange {
    /// First block in the range (inclusive).
    start_height: BlockHeight,
    /// Last block in the range (inclusive).
    end_height: BlockHeight,
}

/// Source-neutral identity for one immutable chain epoch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChainEpochId(u64);

impl ChainEpochId {
    /// Constructs a chain-epoch identity from the source's monotonic value.
    #[must_use]
    pub const fn new(epoch_value: u64) -> Self {
        Self(epoch_value)
    }

    /// Returns the source-provided monotonic value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Canonical block identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockId {
    /// Block height.
    pub height: BlockHeight,
    /// Consensus block hash in Zally's canonical byte order.
    pub hash: BlockHash,
}

/// One source-authenticated immutable chain epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChainEpoch {
    id: ChainEpochId,
    network: Network,
    visible_tip: BlockId,
    settled_tip: BlockId,
}

impl ChainEpoch {
    /// Constructs an epoch when its settled tip does not exceed its visible tip.
    #[must_use]
    pub fn new(
        id: ChainEpochId,
        network: Network,
        visible_tip: BlockId,
        settled_tip: BlockId,
    ) -> Option<Self> {
        if settled_tip.height.as_u32() > visible_tip.height.as_u32()
            || (settled_tip.height.as_u32() == visible_tip.height.as_u32()
                && settled_tip.hash.as_bytes() != visible_tip.hash.as_bytes())
        {
            None
        } else {
            Some(Self {
                id,
                network,
                visible_tip,
                settled_tip,
            })
        }
    }

    /// Returns the immutable source epoch identity.
    #[must_use]
    pub const fn id(self) -> ChainEpochId {
        self.id
    }

    /// Returns the network authenticated by this epoch.
    #[must_use]
    pub const fn network(self) -> Network {
        self.network
    }

    /// Returns the epoch's visible tip.
    #[must_use]
    pub const fn visible_tip(self) -> BlockId {
        self.visible_tip
    }

    /// Returns the epoch's settled tip.
    #[must_use]
    pub const fn settled_tip(self) -> BlockId {
        self.settled_tip
    }
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
    pub fn block_count(&self) -> u64 {
        u64::from(self.end_height.as_u32()) - u64::from(self.start_height.as_u32()) + 1
    }

    /// Returns the first block in the range.
    #[must_use]
    pub const fn start_height(self) -> BlockHeight {
        self.start_height
    }

    /// Returns the last block in the range.
    #[must_use]
    pub const fn end_height(self) -> BlockHeight {
        self.end_height
    }
}

/// Shielded pool selector. Zally's vocabulary for `zcash_protocol::ShieldedPool`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ShieldedPool {
    /// Sapling pool.
    Sapling,
    /// Orchard pool.
    Orchard,
    /// Ironwood pool.
    Ironwood,
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
    /// Height of the block that completed this subtree. `put_*_subtree_roots`
    /// records it as the shard checkpoint, so a wallet can witness notes in the
    /// subtree without scanning every block it spans.
    pub completing_block_height: BlockHeight,
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
    pub value_zat: Zatoshis,
    /// Block height at which the output was mined.
    pub confirmed_at_height: BlockHeight,
    /// Output `scriptPubKey` bytes.
    pub script_pub_key_bytes: Vec<u8>,
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
    /// A non-reorg commit advanced the visible chain or its settled prefix.
    ChainCommitted {
        /// Source-authenticated committed epoch and exact range.
        committed: ChainEpochCommitted,
    },
    /// Reorg detected: reverted blocks were replaced.
    ChainReorged {
        /// Source-authenticated epoch and range that were invalidated.
        reverted: ChainRangeReverted,
        /// Source-authenticated replacement epoch and range.
        committed: ChainEpochCommitted,
    },
}

/// Durable range committed by one chain event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChainEpochCommitted {
    /// Chain epoch visible after the commit.
    pub chain_epoch: ChainEpoch,
    /// Inclusive block range included in the commit.
    pub block_range: BlockHeightRange,
}

/// Previously visible range invalidated by a reorg.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChainRangeReverted {
    /// Chain epoch that contained the reverted range.
    pub chain_epoch: ChainEpoch,
    /// Inclusive block range invalidated by the transition.
    pub block_range: BlockHeightRange,
}

/// Opaque cursor for resuming a chain-event stream.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChainEventCursor {
    cursor_bytes: Vec<u8>,
}

impl ChainEventCursor {
    /// Creates a cursor from bytes returned by a chain source.
    #[must_use]
    pub fn from_bytes(cursor_bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            cursor_bytes: cursor_bytes.into(),
        }
    }

    /// Returns the opaque cursor bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.cursor_bytes
    }

    /// Consumes the cursor and returns the opaque bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.cursor_bytes
    }
}

/// Explicit start position for a chain-event subscription.
///
/// A subscriber states its intent rather than overloading an absent cursor.
/// [`Self::AfterCursor`] resumes strictly after a durably applied cursor: the
/// reconnect path once at least one event has been delivered.
/// [`Self::EarliestRetained`] replays the source's retained window from its
/// floor: the bootstrap path for a subscription that holds no cursor yet.
/// [`Self::LiveTail`] resolves once at subscribe time to the current stream
/// head and delivers only events applied afterwards.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ChainEventStreamStart {
    /// Resume strictly after this cursor.
    AfterCursor(ChainEventCursor),
    /// Replay from the source's earliest retained event.
    EarliestRetained,
    /// Start at the stream head resolved at subscribe time.
    LiveTail,
}

/// Safe recovery position for an expired chain-event cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ChainEventCursorRecovery {
    /// Discard the expired cursor and replay from the source retention floor.
    EarliestRetained,
}

/// Cursor-bound chain event returned to wallet consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct ChainEventEnvelope {
    /// Cursor for resuming strictly after this event.
    pub cursor: ChainEventCursor,
    /// Monotonic sequence in this event stream.
    pub event_sequence: u64,
    /// Full source-authenticated chain epoch visible after this event.
    pub chain_epoch: ChainEpoch,
    /// Source-neutral chain transition.
    pub event: ChainEvent,
}

impl ChainEventEnvelope {
    /// Constructs a cursor-bound chain event.
    #[must_use]
    pub const fn new(
        cursor: ChainEventCursor,
        event_sequence: u64,
        chain_epoch: ChainEpoch,
        event: ChainEvent,
    ) -> Self {
        Self {
            cursor,
            event_sequence,
            chain_epoch,
            event,
        }
    }
}

/// Stream of compact blocks. Each item is exactly one block.
pub type CompactBlockStream =
    Pin<Box<dyn Stream<Item = Result<CompactBlockArtifact, ChainSourceError>> + Send>>;

/// Stream of cursor-bound chain events.
pub type ChainEventEnvelopeStream =
    Pin<Box<dyn Stream<Item = Result<ChainEventEnvelope, ChainSourceError>> + Send>>;

/// Chain-read plane.
///
/// Testkit consumers use `zally_testkit::MockChainSource`. A `ZinderChainSource`
/// implementation is available behind the `zinder` cargo feature. Implementations route
/// blocking work through `tokio::task::spawn_blocking` and emit Zally-vocabulary errors.
#[async_trait]
pub trait ChainSource: Send + Sync + 'static {
    /// Network this chain source is bound to.
    fn network(&self) -> Network;

    /// Acquires one immutable chain epoch. Every artifact read in a sync attempt receives
    /// this value so visible and settled tips cannot drift across source epochs.
    async fn current_epoch(&self) -> Result<ChainEpoch, ChainSourceError>;

    /// Streams compact blocks in `block_range` (inclusive on both ends).
    async fn compact_blocks(
        &self,
        chain_epoch: ChainEpoch,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError>;

    /// Returns the canonical tree state at `block_height`.
    async fn tree_state_at(
        &self,
        chain_epoch: ChainEpoch,
        block_height: BlockHeight,
    ) -> Result<TreeStateArtifact, ChainSourceError>;

    /// Returns subtree roots for `pool` starting at `start_index`.
    async fn subtree_roots(
        &self,
        chain_epoch: ChainEpoch,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError>;

    /// Looks up a transaction.
    async fn transaction_status(&self, tx_id: TxId) -> Result<TransactionStatus, ChainSourceError>;

    /// Returns the complete unspent set for a transparent address at one chain epoch.
    ///
    /// Takes the address as raw `scriptPubKey` bytes so implementations stay free of any
    /// particular address-encoding crate.
    async fn transparent_utxos(
        &self,
        chain_epoch: ChainEpoch,
        script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError>;

    /// Subscribes to cursor-bound chain events from an explicit `start`.
    async fn chain_event_envelopes(
        &self,
        start: ChainEventStreamStart,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_id(height: u32, hash_byte: u8) -> BlockId {
        BlockId {
            height: BlockHeight::from(height),
            hash: BlockHash::from_bytes([hash_byte; 32]),
        }
    }

    #[test]
    fn chain_epoch_enforces_tip_order_and_same_height_identity() {
        let id = ChainEpochId::new(7);
        let network = Network::regtest();
        assert!(ChainEpoch::new(id, network, block_id(10, 1), block_id(9, 2)).is_some());
        assert!(ChainEpoch::new(id, network, block_id(10, 1), block_id(10, 1)).is_some());
        assert!(ChainEpoch::new(id, network, block_id(9, 1), block_id(10, 1)).is_none());
        assert!(ChainEpoch::new(id, network, block_id(10, 1), block_id(10, 2)).is_none());
    }

    #[test]
    fn block_height_range_counts_the_full_u32_domain() {
        let Some(range) = BlockHeightRange::new(BlockHeight::from(0), BlockHeight::from(u32::MAX))
        else {
            unreachable!("the full block-height domain is ordered")
        };
        assert_eq!(range.block_count(), u64::from(u32::MAX) + 1);
    }
}
