//! `ChainSource` trait and the supporting domain vocabulary.

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use zally_core::{BlockHeight, Network, TxId};

use crate::error::ChainSourceError;

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

/// Shielded pool selector. Zally's vocabulary for `zcash_protocol::ShieldedPool`.
///
/// `Ironwood` is not yet reachable through [`ChainSource::subtree_roots`]: the
/// `zinder` backend has no query path for it and returns
/// `ChainSourceError::ShieldedPoolUnsupported`.
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
    pub value_zat: u64,
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
    /// Safe chain tip advanced (no reorg). Carries the new safe-tip height
    /// the wallet may scan to.
    SafeChainTipAdvanced {
        /// Block range that was committed.
        committed_range: BlockHeightRange,
        /// New safe chain tip height.
        new_safe_chain_tip_height: BlockHeight,
    },
    /// Reorg detected: reverted blocks were replaced.
    ChainReorged {
        /// Block range that was reverted.
        reverted_range: BlockHeightRange,
        /// Block range that was committed in its place.
        committed_range: BlockHeightRange,
        /// New safe chain tip height after the reorg.
        new_safe_chain_tip_height: BlockHeight,
    },
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

/// Cursor-bound chain event returned to wallet consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct ChainEventEnvelope {
    /// Cursor for resuming strictly after this event.
    pub cursor: ChainEventCursor,
    /// Monotonic sequence in this event stream.
    pub event_sequence: u64,
    /// Safe chain tip height reported with this event (the wallet's scan
    /// ceiling at delivery time).
    pub safe_chain_tip_height: BlockHeight,
    /// Source-neutral chain transition.
    pub event: ChainEvent,
}

impl ChainEventEnvelope {
    /// Constructs a cursor-bound chain event.
    #[must_use]
    pub const fn new(
        cursor: ChainEventCursor,
        event_sequence: u64,
        safe_chain_tip_height: BlockHeight,
        event: ChainEvent,
    ) -> Self {
        Self {
            cursor,
            event_sequence,
            safe_chain_tip_height,
            event,
        }
    }
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

    /// Highest height the wallet may safely scan to.
    ///
    /// Contract: the chain source guarantees that for any height `h` at or
    /// below the returned value, (a) the compact block at `h` is available
    /// and (b) a tree-state checkpoint is available at some height `k` with
    /// `h - k < REWIND_CAP` (the librustzcash 100-block rewind cap). The
    /// wallet uses this as its scan ceiling and as the anchor frontier for
    /// note-commitment-tree lookups. Always trails [`Self::chain_tip`] by
    /// at least `reorg_window_blocks` (default 100).
    async fn safe_chain_tip(&self) -> Result<BlockHeight, ChainSourceError>;

    /// Current chain head height: the highest block the chain source has
    /// observed, including blocks still inside the reorg window.
    ///
    /// The wallet uses this for transaction-construction math: target
    /// height (`chain_tip + 1`) and expiry height
    /// (`target + tx_expiry_delta`). Both values must reference the
    /// chain's current best block, not the safe-tip floor, so submitted
    /// transactions land before consensus rejects them as
    /// `BadExpiryHeight`. Anchor selection and scan ceilings must use
    /// [`Self::safe_chain_tip`] instead; mixing the two is the wedge
    /// class ADR-0005 closed.
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

    /// Returns the complete unspent set for a transparent address at one chain epoch.
    ///
    /// Takes the address as raw `scriptPubKey` bytes so implementations stay free of any
    /// particular address-encoding crate.
    async fn transparent_utxos(
        &self,
        script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError>;

    /// Subscribes to cursor-bound chain events from an explicit `start`.
    async fn chain_event_envelopes(
        &self,
        start: ChainEventStreamStart,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError>;
}
