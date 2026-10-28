//! Programmable in-memory `ChainSource` fixture.
//!
//! `MockChainSource` lets a test:
//!
//! - set the visible chain tip via [`MockChainSourceHandle::advance_tip`],
//! - replace every block from a fork point onward and announce it as a
//!   [`ChainEvent::ChainReorged`] via [`MockChainSourceHandle::trigger_reorg`], or replace them
//!   without an announcement via [`MockChainSourceHandle::reorg_silently`],
//! - serve scannable compact blocks by default, so scan progress commits, with caller-supplied
//!   transactions and commitment trees at chosen heights via
//!   [`MockChainSourceHandle::mine_transactions`],
//! - serve a caller-supplied tree state at its height via
//!   [`MockChainSourceHandle::serve_tree_state`], so tree-root verification sees
//!   chain roots the test controls,
//! - resume chain events from a cursor over a retained event window whose floor
//!   [`MockChainSourceHandle::expire_chain_events_through`] raises,
//! - report a reorg window other than 1,000 blocks via
//!   [`MockChainSourceHandle::set_reorg_window_blocks`],
//! - report a transaction mined at the lowest canonical block, at or below the visible tip,
//!   that [`MockChainSourceHandle::mine_transactions`] put it in, and every other transaction
//!   not found.
//!
//! Blocks on the original branch carry all-zero hashes, so tests that serve their own
//! all-zero tree states keep chaining. Every block a reorg replaces carries a hash derived
//! from its height and its branch, so a wallet holding the abandoned branch sees a
//! continuity break when it scans the replacement.
//!
//! The handle returned by [`MockChainSource::handle`] is `Clone` and shares state with the
//! original mock; tests drive the mock through the handle while passing the mock itself to
//! `Wallet::sync`.

use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::Stream;
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use zally_chain::{
    BlockHeightRange, BlockId, ChainEpoch, ChainEpochCommitted, ChainEpochId, ChainEvent,
    ChainEventCursor, ChainEventCursorRecovery, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainEventStreamStart, ChainRangeReverted, ChainSource, ChainSourceError, CompactBlockStream,
    ShieldedPool, SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};
use zally_core::{
    BlockHash, BlockHeight, CompactBlockArtifact, CompactChainMetadata, CompactTransaction,
    Network, TreeStateArtifact, TxId,
};
use zcash_protocol::consensus::{NetworkUpgrade, Parameters as _};

const EVENT_CHANNEL_CAPACITY: usize = 256;
/// Reorg window the mock reports until a test sets another: the deepest reorg Zebra produces.
const DEFAULT_REORG_WINDOW_BLOCKS: u32 = 1_000;

struct MockState {
    network: Network,
    visible_tip_height: BlockHeight,
    settled_tip_height: BlockHeight,
    epoch_id: u64,
    current_epoch_failures: Vec<ChainSourceError>,
    transparent_utxo_failures: Vec<ChainSourceError>,
    subtree_failures: Vec<ChainSourceError>,
    compact_failures: Vec<ChainSourceError>,
    tree_state_failure_heights: Vec<u32>,
    tree_state_error_queue: Vec<(u32, ChainSourceError)>,
    failures_consumed: u32,
    acquired_epoch_ids: Vec<u64>,
    artifact_epoch_ids: Vec<u64>,
    is_serving_compact_blocks: bool,
    tree_states_by_height: HashMap<u32, TreeStateArtifact>,
    /// Branch tag per fork point, keyed by the first height the branch replaced.
    branch_tags_by_start: BTreeMap<u32, u8>,
    mined_blocks: BTreeMap<u32, Vec<MinedBlock>>,
    retained_events: Vec<ChainEventEnvelope>,
    next_event_sequence: u64,
    retained_from_sequence: u64,
    subscription_starts: Vec<ChainEventStreamStart>,
    reorg_window_blocks: u32,
}

/// Commitment trees as they stand after one mined block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MockCommitmentTrees {
    /// `z_gettreestate` encoding of the Sapling frontier.
    pub sapling_frontier_bytes: Vec<u8>,
    /// `z_gettreestate` encoding of the Orchard frontier.
    pub orchard_frontier_bytes: Vec<u8>,
    /// `z_gettreestate` encoding of the Ironwood frontier.
    pub ironwood_frontier_bytes: Vec<u8>,
    /// Tree sizes carried by every compact block from this one until the next mined block.
    pub chain_metadata: CompactChainMetadata,
}

#[derive(Clone)]
struct MinedBlock {
    branch_tag: u8,
    transactions: Vec<CompactTransaction>,
    commitment_trees: MockCommitmentTrees,
}

impl MockState {
    fn branch_tag_at(&self, height: u32) -> u8 {
        self.branch_tags_by_start
            .range(..=height)
            .next_back()
            .map_or(0, |(_, tag)| *tag)
    }

    fn block_hash_at(&self, height: u32) -> BlockHash {
        branch_block_hash(height, self.branch_tag_at(height))
    }

    /// The canonical mined block at `height`, if the test mined one on the current branch.
    fn canonical_mined_block(&self, height: u32) -> Option<&MinedBlock> {
        let tag = self.branch_tag_at(height);
        self.mined_blocks
            .get(&height)
            .and_then(|blocks| blocks.iter().rev().find(|block| block.branch_tag == tag))
    }

    /// Commitment trees after the highest canonical mined block at or below `height`.
    fn commitment_trees_at(&self, height: u32) -> Option<&MockCommitmentTrees> {
        self.mined_blocks
            .range(..=height)
            .rev()
            .find_map(|(mined_height, _)| self.canonical_mined_block(*mined_height))
            .map(|block| &block.commitment_trees)
    }

    fn compact_block(&self, height: u32) -> CompactBlockArtifact {
        let chain_metadata = self
            .commitment_trees_at(height)
            .map_or(EMPTY_CHAIN_METADATA, |trees| trees.chain_metadata);
        CompactBlockArtifact {
            height: BlockHeight::from(height),
            block_hash: self.block_hash_at(height),
            previous_block_hash: self.block_hash_at(height.saturating_sub(1)),
            block_time_seconds: height,
            transactions: self
                .canonical_mined_block(height)
                .map(|block| block.transactions.clone())
                .unwrap_or_default(),
            chain_metadata,
        }
    }

    /// Replaces every block from `fork_start_height` onward with a new branch.
    fn replace_branch_from(&mut self, fork_start_height: u32) {
        let next_tag = self
            .branch_tags_by_start
            .values()
            .copied()
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.branch_tags_by_start.split_off(&fork_start_height);
        self.branch_tags_by_start
            .insert(fork_start_height, next_tag);
    }

    fn record_event(&mut self, event: ChainEvent) -> Option<ChainEventEnvelope> {
        let chain_epoch = event_chain_epoch(&event).ok()?;
        let sequence = self.next_event_sequence;
        self.next_event_sequence = self.next_event_sequence.saturating_add(1);
        let envelope = ChainEventEnvelope::new(
            ChainEventCursor::from_bytes(sequence.to_be_bytes().to_vec()),
            sequence,
            chain_epoch,
            event,
        );
        self.retained_events.push(envelope.clone());
        Some(envelope)
    }
}

const EMPTY_CHAIN_METADATA: CompactChainMetadata = CompactChainMetadata {
    sapling_commitment_tree_size: 0,
    orchard_commitment_tree_size: 0,
    ironwood_commitment_tree_size: 0,
};

/// Hash of the block at `height` on branch `branch_tag`. The original branch keeps the
/// all-zero hash every earlier fixture chains against.
fn branch_block_hash(height: u32, branch_tag: u8) -> BlockHash {
    if branch_tag == 0 {
        return BlockHash::from_bytes([0; 32]);
    }
    let mut bytes = [0x5a; 32];
    bytes[0] = branch_tag;
    bytes[1..5].copy_from_slice(&height.to_le_bytes());
    BlockHash::from_bytes(bytes)
}

/// In-memory `ChainSource` fixture.
pub struct MockChainSource {
    state: Arc<Mutex<MockState>>,
    event_tx: broadcast::Sender<ChainEventEnvelope>,
}

impl MockChainSource {
    /// Constructs a new mock at chain tip = `0` for `network`.
    #[must_use]
    pub fn new(network: Network) -> Self {
        let (event_tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            state: Arc::new(Mutex::new(MockState {
                network,
                visible_tip_height: BlockHeight::GENESIS,
                settled_tip_height: BlockHeight::GENESIS,
                epoch_id: 0,
                current_epoch_failures: Vec::new(),
                transparent_utxo_failures: Vec::new(),
                subtree_failures: Vec::new(),
                compact_failures: Vec::new(),
                tree_state_failure_heights: Vec::new(),
                tree_state_error_queue: Vec::new(),
                failures_consumed: 0,
                acquired_epoch_ids: Vec::new(),
                artifact_epoch_ids: Vec::new(),
                is_serving_compact_blocks: true,
                tree_states_by_height: HashMap::new(),
                branch_tags_by_start: BTreeMap::new(),
                mined_blocks: BTreeMap::new(),
                retained_events: Vec::new(),
                next_event_sequence: 1,
                retained_from_sequence: 1,
                subscription_starts: Vec::new(),
                reorg_window_blocks: DEFAULT_REORG_WINDOW_BLOCKS,
            })),
            event_tx,
        }
    }

    /// Returns a programmable handle that shares state with this mock.
    #[must_use]
    pub fn handle(&self) -> MockChainSourceHandle {
        MockChainSourceHandle {
            state: Arc::clone(&self.state),
            event_tx: self.event_tx.clone(),
        }
    }

    fn validate_epoch(&self, chain_epoch: ChainEpoch) -> Result<(), ChainSourceError> {
        let (network, epoch_id) = {
            let guard = self.state.lock();
            (guard.network, guard.epoch_id)
        };
        if chain_epoch.network() != network {
            return Err(ChainSourceError::NetworkMismatch {
                chain_source_network: network,
                requested_network: chain_epoch.network(),
            });
        }
        if chain_epoch.id() != ChainEpochId::new(epoch_id) {
            return Err(ChainSourceError::ChainEpochPinUnavailable);
        }
        Ok(())
    }
}

/// Programmable handle for driving a [`MockChainSource`] from a test.
#[derive(Clone)]
pub struct MockChainSourceHandle {
    state: Arc<Mutex<MockState>>,
    event_tx: broadcast::Sender<ChainEventEnvelope>,
}

impl MockChainSourceHandle {
    /// Sets the settled tip to `new_settled_tip_height` and emits `SettledTipAdvanced`.
    pub fn advance_tip(&self, new_settled_tip_height: BlockHeight) {
        let committed = {
            let mut guard = self.state.lock();
            let prior_tip = guard.settled_tip_height;
            guard.visible_tip_height = new_settled_tip_height;
            guard.settled_tip_height = new_settled_tip_height;
            guard.epoch_id = guard.epoch_id.saturating_add(1);
            let chain_epoch = chain_epoch_from_state(&guard);
            drop(guard);
            let range = BlockHeightRange::new(
                BlockHeight::from(prior_tip.as_u32().saturating_add(1)),
                new_settled_tip_height,
            );
            range.map(|block_range| ChainEpochCommitted {
                chain_epoch,
                block_range,
            })
        };
        if let Some(committed) = committed {
            self.publish(ChainEvent::ChainCommitted { committed });
        }
    }

    fn publish(&self, event: ChainEvent) {
        let envelope = self.state.lock().record_event(event);
        if let Some(envelope) = envelope {
            let _ = self.event_tx.send(envelope);
        }
    }

    /// Advances the visible tip to `visible_tip` and the settled tip to `settled_tip` on the
    /// current branch, and announces the commit. Leaves the mock unchanged and returns
    /// `false` when `settled_tip > visible_tip`.
    #[must_use]
    pub fn commit_tips(&self, visible_tip: BlockHeight, settled_tip: BlockHeight) -> bool {
        let committed = {
            let mut guard = self.state.lock();
            if settled_tip > visible_tip {
                return false;
            }
            let prior_tip = guard.visible_tip_height;
            guard.visible_tip_height = visible_tip;
            guard.settled_tip_height = settled_tip;
            guard.epoch_id = guard.epoch_id.saturating_add(1);
            let chain_epoch = chain_epoch_from_state(&guard);
            drop(guard);
            BlockHeightRange::new(
                BlockHeight::from(prior_tip.as_u32().saturating_add(1)),
                visible_tip,
            )
            .map(|block_range| ChainEpochCommitted {
                chain_epoch,
                block_range,
            })
        };
        if let Some(committed) = committed {
            self.publish(ChainEvent::ChainCommitted { committed });
        }
        true
    }

    /// Sets a coherent visible/settled pair without emitting a chain event.
    ///
    /// Returns `false` and leaves the mock unchanged when `settled_tip > visible_tip`.
    #[must_use]
    pub fn set_chain_tips(&self, visible_tip: BlockHeight, settled_tip: BlockHeight) -> bool {
        if settled_tip > visible_tip {
            return false;
        }
        let mut guard = self.state.lock();
        guard.visible_tip_height = visible_tip;
        guard.settled_tip_height = settled_tip;
        guard.epoch_id = guard.epoch_id.saturating_add(1);
        true
    }

    /// Replaces every block from `reorg_from_height` onward with a new branch reaching
    /// `new_settled_tip_height`, and announces the reorg as a [`ChainEvent::ChainReorged`].
    ///
    /// The fork point is `reorg_from_height - 1`.
    pub fn trigger_reorg(
        &self,
        reorg_from_height: BlockHeight,
        new_settled_tip_height: BlockHeight,
    ) {
        let (prior_tip, reverted_epoch, committed_epoch) = {
            let mut guard = self.state.lock();
            let prior_tip = guard.settled_tip_height;
            let reverted_epoch = chain_epoch_from_state(&guard);
            guard.replace_branch_from(reorg_from_height.as_u32());
            guard.visible_tip_height = new_settled_tip_height;
            guard.settled_tip_height = new_settled_tip_height;
            guard.epoch_id = guard.epoch_id.saturating_add(1);
            let committed_epoch = chain_epoch_from_state(&guard);
            drop(guard);
            (prior_tip, reverted_epoch, committed_epoch)
        };
        let reverted = BlockHeightRange::new(reorg_from_height, prior_tip);
        let committed = BlockHeightRange::new(reorg_from_height, new_settled_tip_height);
        if let (Some(reverted_range), Some(committed_range)) = (reverted, committed) {
            self.publish(ChainEvent::ChainReorged {
                reverted: ChainRangeReverted {
                    chain_epoch: reverted_epoch,
                    block_range: reverted_range,
                },
                committed: ChainEpochCommitted {
                    chain_epoch: committed_epoch,
                    block_range: committed_range,
                },
            });
        }
    }

    /// Replaces every block from `reorg_from_height` onward with a new branch reaching
    /// `new_tip_height` without announcing it, as a source that missed the reorg would.
    pub fn reorg_silently(&self, reorg_from_height: BlockHeight, new_tip_height: BlockHeight) {
        let mut guard = self.state.lock();
        guard.replace_branch_from(reorg_from_height.as_u32());
        guard.visible_tip_height = new_tip_height;
        guard.settled_tip_height = new_tip_height;
        guard.epoch_id = guard.epoch_id.saturating_add(1);
    }

    /// Mines `transactions` into the block at `height` on the branch that currently holds
    /// that height. `commitment_trees` describes the trees after the block; later blocks
    /// carry the same trees until the next mined block.
    ///
    /// A reorg that replaces `height` abandons the block; mine the transactions again to
    /// include them on the replacement branch.
    pub fn mine_transactions(
        &self,
        height: BlockHeight,
        transactions: Vec<CompactTransaction>,
        commitment_trees: MockCommitmentTrees,
    ) {
        let mut guard = self.state.lock();
        let branch_tag = guard.branch_tag_at(height.as_u32());
        guard
            .mined_blocks
            .entry(height.as_u32())
            .or_default()
            .push(MinedBlock {
                branch_tag,
                transactions,
                commitment_trees,
            });
    }

    /// Hash of the block the mock currently serves at `height`.
    #[must_use]
    pub fn block_hash_at(&self, height: BlockHeight) -> BlockHash {
        self.state.lock().block_hash_at(height.as_u32())
    }

    /// Drops every retained chain event up to and including `sequence`, so a cursor at or
    /// before it resumes with [`ChainSourceError::ChainEventCursorExpired`].
    pub fn expire_chain_events_through(&self, sequence: u64) {
        let mut guard = self.state.lock();
        guard.retained_from_sequence = sequence.saturating_add(1);
        let floor = guard.retained_from_sequence;
        guard
            .retained_events
            .retain(|envelope| envelope.event_sequence >= floor);
    }

    /// Sequence of the most recent chain event, `0` before the first.
    #[must_use]
    pub fn latest_chain_event_sequence(&self) -> u64 {
        self.state.lock().next_event_sequence.saturating_sub(1)
    }

    /// Start positions of every chain-event subscription, in call order.
    #[must_use]
    pub fn chain_event_subscription_starts(&self) -> Vec<ChainEventStreamStart> {
        self.state.lock().subscription_starts.clone()
    }

    /// Queues `count` consecutive failures for `current_epoch` calls. Each subsequent call pops
    /// one failure off the queue; once empty, calls succeed normally. Failures are returned
    /// in the order they were queued.
    ///
    /// `produce_error` is invoked once per queued failure, so callers can either return the
    /// same error each time (`|| ChainSourceError::Unavailable { ... }`) or vary the error
    /// per attempt. The closure avoids requiring `Clone` on the error type, which would
    /// otherwise force [`ChainSourceError::Indexer`] to carry an `Arc<IndexerError>`.
    pub fn fail_current_epoch_next(
        &self,
        count: u32,
        mut produce_error: impl FnMut() -> ChainSourceError,
    ) {
        let mut guard = self.state.lock();
        for _ in 0..count {
            guard.current_epoch_failures.push(produce_error());
        }
    }

    /// Queues `count` consecutive failures for `transparent_utxos` calls. Mirrors
    /// [`Self::fail_current_epoch_next`]: each call pops one failure off the queue; once empty,
    /// calls succeed normally.
    pub fn fail_transparent_utxos_next(
        &self,
        count: u32,
        mut produce_error: impl FnMut() -> ChainSourceError,
    ) {
        let mut guard = self.state.lock();
        for _ in 0..count {
            guard.transparent_utxo_failures.push(produce_error());
        }
    }

    /// Expires the current epoch during the next subtree-root read.
    pub fn expire_epoch_on_next_subtree_read(&self) {
        self.state
            .lock()
            .subtree_failures
            .push(ChainSourceError::ChainEpochPinUnavailable);
    }

    /// Expires the current epoch during the next compact-block range read.
    pub fn expire_epoch_on_next_compact_read(&self) {
        self.state
            .lock()
            .compact_failures
            .push(ChainSourceError::ChainEpochPinUnavailable);
    }

    /// Expires the current epoch during the next tree-state read at `block_height`.
    ///
    /// Keyed on the height because one scan cycle reads two tree states: the anchor below
    /// its range, then the one it compares its roots against.
    pub fn expire_epoch_on_tree_state_at(&self, block_height: BlockHeight) {
        self.state
            .lock()
            .tree_state_failure_heights
            .push(block_height.as_u32());
    }

    /// Queues `count` consecutive failures for `tree_state_at` reads at `block_height`,
    /// without rotating the mock's epoch. Mirrors [`Self::fail_transparent_utxos_next`] but
    /// keyed on height, for injecting a fault after a scan chunk has already committed (the
    /// tree-root comparison is the only chain read left in that window).
    pub fn fail_tree_state_at_next(
        &self,
        block_height: BlockHeight,
        count: u32,
        mut produce_error: impl FnMut() -> ChainSourceError,
    ) {
        let mut guard = self.state.lock();
        for _ in 0..count {
            guard
                .tree_state_error_queue
                .push((block_height.as_u32(), produce_error()));
        }
    }

    /// Epoch IDs returned by `current_epoch`, in call order.
    #[must_use]
    pub fn acquired_epoch_ids(&self) -> Vec<u64> {
        self.state.lock().acquired_epoch_ids.clone()
    }

    /// Epoch IDs supplied to artifact reads, in call order.
    #[must_use]
    pub fn artifact_epoch_ids(&self) -> Vec<u64> {
        self.state.lock().artifact_epoch_ids.clone()
    }

    /// Streams a transactionless compact block per requested height instead of an empty
    /// stream, so `Wallet::sync` runs the real scanner and commits scan progress.
    ///
    /// Every block carries an all-zero hash and previous hash, matching the all-zero block
    /// hash in the mock's `tree_state_at`, so the scanner's continuity checks pass.
    pub fn serve_compact_blocks(&self) {
        self.state.lock().is_serving_compact_blocks = true;
    }

    /// Serves `tree_state` from `tree_state_at` for the height it carries, instead of the
    /// default all-empty tree state.
    pub fn serve_tree_state(&self, tree_state: TreeStateArtifact) {
        let height = tree_state.height.as_u32();
        self.state
            .lock()
            .tree_states_by_height
            .insert(height, tree_state);
    }

    /// Serves an artifact for a requested height independent of the height it claims.
    ///
    /// This intentionally malformed fixture hook exercises consumer trust-boundary checks.
    pub fn serve_tree_state_for(
        &self,
        requested_height: BlockHeight,
        tree_state: TreeStateArtifact,
    ) {
        self.state
            .lock()
            .tree_states_by_height
            .insert(requested_height.as_u32(), tree_state);
    }

    /// Number of failures consumed since the mock was constructed.
    #[must_use]
    pub fn failures_consumed(&self) -> u32 {
        self.state.lock().failures_consumed
    }

    /// Sets the reorg window the mock reports through [`ChainSource::reorg_window_blocks`].
    /// The mock starts at 1,000 blocks.
    pub fn set_reorg_window_blocks(&self, reorg_window_blocks: u32) {
        self.state.lock().reorg_window_blocks = reorg_window_blocks;
    }
}

fn chain_epoch_from_state(state: &MockState) -> ChainEpoch {
    ChainEpoch::new(
        ChainEpochId::new(state.epoch_id),
        state.network,
        BlockId {
            height: state.visible_tip_height,
            hash: state.block_hash_at(state.visible_tip_height.as_u32()),
        },
        BlockId {
            height: state.settled_tip_height,
            hash: state.block_hash_at(state.settled_tip_height.as_u32()),
        },
    )
    .unwrap_or_else(|| unreachable!("mock tips are validated before epoch construction"))
}

#[async_trait]
impl ChainSource for MockChainSource {
    fn network(&self) -> Network {
        self.state.lock().network
    }

    async fn current_epoch(&self) -> Result<ChainEpoch, ChainSourceError> {
        let mut guard = self.state.lock();
        if !guard.current_epoch_failures.is_empty() {
            let injected = guard.current_epoch_failures.remove(0);
            guard.failures_consumed = guard.failures_consumed.saturating_add(1);
            return Err(injected);
        }
        let epoch_id = guard.epoch_id;
        guard.acquired_epoch_ids.push(epoch_id);
        let chain_epoch = chain_epoch_from_state(&guard);
        drop(guard);
        Ok(chain_epoch)
    }

    async fn compact_blocks(
        &self,
        chain_epoch: ChainEpoch,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        {
            let mut guard = self.state.lock();
            guard.artifact_epoch_ids.push(chain_epoch.id().value());
            if !guard.compact_failures.is_empty() {
                let error = guard.compact_failures.remove(0);
                guard.epoch_id = guard.epoch_id.saturating_add(1);
                drop(guard);
                return Err(error);
            }
        }
        self.validate_epoch(chain_epoch)?;
        let guard = self.state.lock();
        let tip = guard.visible_tip_height;
        if block_range.end_height().as_u32() > tip.as_u32() {
            return Err(ChainSourceError::BlockHeightAboveVisibleTip {
                requested_height: block_range.end_height(),
                visible_tip_height: tip,
            });
        }
        let blocks: Vec<Result<CompactBlockArtifact, ChainSourceError>> =
            if guard.is_serving_compact_blocks {
                (block_range.start_height().as_u32()..=block_range.end_height().as_u32())
                    .map(|height| Ok(guard.compact_block(height)))
                    .collect()
            } else {
                Vec::new()
            };
        drop(guard);
        let stream: Pin<
            Box<dyn Stream<Item = Result<CompactBlockArtifact, ChainSourceError>> + Send>,
        > = Box::pin(tokio_stream::iter(blocks));
        Ok(stream)
    }

    async fn tree_state_at(
        &self,
        chain_epoch: ChainEpoch,
        block_height: BlockHeight,
    ) -> Result<TreeStateArtifact, ChainSourceError> {
        {
            let mut guard = self.state.lock();
            guard.artifact_epoch_ids.push(chain_epoch.id().value());
            if let Some(index) = guard
                .tree_state_failure_heights
                .iter()
                .position(|height| *height == block_height.as_u32())
            {
                guard.tree_state_failure_heights.remove(index);
                guard.epoch_id = guard.epoch_id.saturating_add(1);
                guard.failures_consumed = guard.failures_consumed.saturating_add(1);
                drop(guard);
                return Err(ChainSourceError::ChainEpochPinUnavailable);
            }
            if let Some(index) = guard
                .tree_state_error_queue
                .iter()
                .position(|(height, _)| *height == block_height.as_u32())
            {
                let (_, error) = guard.tree_state_error_queue.remove(index);
                guard.failures_consumed = guard.failures_consumed.saturating_add(1);
                drop(guard);
                return Err(error);
            }
        }
        self.validate_epoch(chain_epoch)?;
        let guard = self.state.lock();
        if let Some(tree_state) = guard.tree_states_by_height.get(&block_height.as_u32()) {
            return Ok(tree_state.clone());
        }
        let network = guard.network;
        let block_hash = guard.block_hash_at(block_height.as_u32());
        if let Some(trees) = guard.commitment_trees_at(block_height.as_u32()) {
            return Ok(TreeStateArtifact {
                network,
                height: block_height,
                block_hash,
                block_time_seconds: block_height.as_u32(),
                sapling_final_state_bytes: trees.sapling_frontier_bytes.clone(),
                orchard_final_state_bytes: trees.orchard_frontier_bytes.clone(),
                ironwood_final_state_bytes: trees.ironwood_frontier_bytes.clone(),
            });
        }
        drop(guard);
        Ok(TreeStateArtifact {
            network,
            height: block_height,
            block_hash,
            block_time_seconds: 0,
            sapling_final_state_bytes: mock_frontier(
                network,
                block_height,
                NetworkUpgrade::Sapling,
            ),
            orchard_final_state_bytes: mock_frontier(network, block_height, NetworkUpgrade::Nu5),
            ironwood_final_state_bytes: mock_frontier(network, block_height, NetworkUpgrade::Nu6_3),
        })
    }

    async fn subtree_roots(
        &self,
        chain_epoch: ChainEpoch,
        _pool: ShieldedPool,
        _start_index: SubtreeIndex,
        _max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
        {
            let mut guard = self.state.lock();
            guard.artifact_epoch_ids.push(chain_epoch.id().value());
            if !guard.subtree_failures.is_empty() {
                let error = guard.subtree_failures.remove(0);
                guard.epoch_id = guard.epoch_id.saturating_add(1);
                drop(guard);
                return Err(error);
            }
        }
        self.validate_epoch(chain_epoch)?;
        Ok(Vec::new())
    }

    async fn transaction_status(&self, tx_id: TxId) -> Result<TransactionStatus, ChainSourceError> {
        let guard = self.state.lock();
        let visible_tip_height = guard.visible_tip_height.as_u32();
        let confirmed_at_height = guard
            .mined_blocks
            .range(..=visible_tip_height)
            .map(|(height, _)| *height)
            .find(|height| {
                guard.canonical_mined_block(*height).is_some_and(|block| {
                    block
                        .transactions
                        .iter()
                        .any(|transaction| transaction.tx_id == tx_id)
                })
            });
        drop(guard);
        Ok(
            confirmed_at_height.map_or(TransactionStatus::NotFound, |height| {
                TransactionStatus::Confirmed {
                    tx_id,
                    confirmed_at_height: BlockHeight::from(height),
                }
            }),
        )
    }

    async fn transparent_utxos(
        &self,
        chain_epoch: ChainEpoch,
        _script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        self.state
            .lock()
            .artifact_epoch_ids
            .push(chain_epoch.id().value());
        self.validate_epoch(chain_epoch)?;
        let mut guard = self.state.lock();
        if !guard.transparent_utxo_failures.is_empty() {
            let injected = guard.transparent_utxo_failures.remove(0);
            guard.failures_consumed = guard.failures_consumed.saturating_add(1);
            drop(guard);
            return Err(injected);
        }
        Ok(Vec::new())
    }

    async fn chain_event_envelopes(
        &self,
        start: ChainEventStreamStart,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
        let receiver = self.event_tx.subscribe();
        let (replay, last_replayed_sequence) = {
            let mut guard = self.state.lock();
            guard.subscription_starts.push(start.clone());
            let latest_sequence = guard.next_event_sequence.saturating_sub(1);
            let resume_after_sequence = match &start {
                ChainEventStreamStart::AfterCursor(cursor) => {
                    let sequence = cursor_sequence(cursor)
                        .filter(|sequence| *sequence <= latest_sequence)
                        .ok_or(ChainSourceError::ChainEventCursorInvalid)?;
                    if sequence.saturating_add(1) < guard.retained_from_sequence {
                        return Err(ChainSourceError::ChainEventCursorExpired {
                            recovery: ChainEventCursorRecovery::EarliestRetained,
                        });
                    }
                    sequence
                }
                ChainEventStreamStart::EarliestRetained => {
                    guard.retained_from_sequence.saturating_sub(1)
                }
                ChainEventStreamStart::LiveTail => latest_sequence,
            };
            let replay: Vec<Result<ChainEventEnvelope, ChainSourceError>> = guard
                .retained_events
                .iter()
                .filter(|envelope| envelope.event_sequence > resume_after_sequence)
                .cloned()
                .map(Ok)
                .collect();
            drop(guard);
            (replay, latest_sequence.max(resume_after_sequence))
        };
        let live = BroadcastStream::new(receiver).filter_map(move |delivery| match delivery {
            Ok(envelope) if envelope.event_sequence > last_replayed_sequence => Some(Ok(envelope)),
            Ok(_) | Err(_) => None,
        });
        Ok(Box::pin(tokio_stream::iter(replay).chain(live)))
    }

    async fn reorg_window_blocks(&self) -> Result<u32, ChainSourceError> {
        Ok(self.state.lock().reorg_window_blocks)
    }
}

fn cursor_sequence(cursor: &ChainEventCursor) -> Option<u64> {
    let bytes: [u8; 8] = cursor.as_bytes().try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn mock_frontier(network: Network, height: BlockHeight, activation: NetworkUpgrade) -> Vec<u8> {
    let height = zcash_protocol::consensus::BlockHeight::from_u32(height.as_u32());
    if network
        .to_parameters()
        .activation_height(activation)
        .is_some_and(|activation_height| height >= activation_height)
    {
        vec![0, 0, 0]
    } else {
        Vec::new()
    }
}

fn event_chain_epoch(event: &ChainEvent) -> Result<ChainEpoch, ChainSourceError> {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "non_exhaustive chain events map unknown variants to the genesis cursor"
    )]
    match event {
        ChainEvent::ChainCommitted { committed } | ChainEvent::ChainReorged { committed, .. } => {
            Ok(committed.chain_epoch)
        }
        _ => Err(ChainSourceError::UnsupportedResponse {
            response: "ChainEvent",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _FuturesStreamExt;

    #[tokio::test]
    async fn mock_current_epoch_advances_on_handle_call() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest());
        let handle = mock.handle();
        assert_eq!(
            mock.current_epoch().await?.settled_tip().height,
            BlockHeight::GENESIS
        );
        handle.advance_tip(BlockHeight::from(10));
        assert_eq!(
            mock.current_epoch().await?.settled_tip().height,
            BlockHeight::from(10)
        );
        Ok(())
    }

    #[tokio::test]
    async fn mock_emits_settled_tip_advanced_event() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest());
        let mut envelopes = mock
            .chain_event_envelopes(ChainEventStreamStart::EarliestRetained)
            .await?;
        let handle = mock.handle();
        handle.advance_tip(BlockHeight::from(3));
        let first = _FuturesStreamExt::next(&mut envelopes).await;
        assert!(matches!(
            first,
            Some(Ok(envelope)) if matches!(
                envelope.event,
                ChainEvent::ChainCommitted { committed } if committed.chain_epoch.settled_tip().height == BlockHeight::from(3)
            )
        ));
        Ok(())
    }

    #[tokio::test]
    async fn mock_emits_chain_reorged_event() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest());
        let handle = mock.handle();
        handle.advance_tip(BlockHeight::from(10));
        let mut envelopes = mock
            .chain_event_envelopes(ChainEventStreamStart::LiveTail)
            .await?;
        handle.trigger_reorg(BlockHeight::from(7), BlockHeight::from(11));
        let event = _FuturesStreamExt::next(&mut envelopes).await;
        assert!(matches!(
            event,
            Some(Ok(envelope)) if matches!(
                envelope.event,
                ChainEvent::ChainReorged { committed, .. } if committed.chain_epoch.settled_tip().height == BlockHeight::from(11)
            )
        ));
        Ok(())
    }

    #[tokio::test]
    async fn mock_compact_blocks_returns_requested_range() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest());
        mock.handle().advance_tip(BlockHeight::from(5));
        let range =
            BlockHeightRange::new(BlockHeight::from(1), BlockHeight::from(5)).ok_or_else(|| {
                ChainSourceError::Unavailable {
                    reason: "range".into(),
                }
            })?;
        let chain_epoch = mock.current_epoch().await?;
        let mut stream = mock.compact_blocks(chain_epoch, range).await?;
        let first = _FuturesStreamExt::next(&mut stream).await.transpose()?;
        assert_eq!(first.map(|block| block.height), Some(BlockHeight::from(1)));
        Ok(())
    }

    #[tokio::test]
    async fn mock_compact_blocks_above_tip_rejected() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest());
        let range = BlockHeightRange::new(BlockHeight::from(1), BlockHeight::from(100))
            .ok_or_else(|| ChainSourceError::Unavailable {
                reason: "range".into(),
            })?;
        let chain_epoch = mock.current_epoch().await?;
        let outcome = mock.compact_blocks(chain_epoch, range).await;
        assert!(matches!(
            outcome,
            Err(ChainSourceError::BlockHeightAboveVisibleTip { .. })
        ));
        Ok(())
    }

    async fn next_sequence(
        envelopes: &mut ChainEventEnvelopeStream,
    ) -> Result<Option<u64>, ChainSourceError> {
        _FuturesStreamExt::next(envelopes)
            .await
            .transpose()
            .map(|envelope| envelope.map(|envelope| envelope.event_sequence))
    }

    #[tokio::test]
    async fn chain_events_resume_after_a_cursor_from_the_retained_window()
    -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest());
        let handle = mock.handle();
        handle.advance_tip(BlockHeight::from(1));
        handle.advance_tip(BlockHeight::from(2));
        handle.advance_tip(BlockHeight::from(3));

        let mut resumed = mock
            .chain_event_envelopes(ChainEventStreamStart::AfterCursor(
                ChainEventCursor::from_bytes(1_u64.to_be_bytes().to_vec()),
            ))
            .await?;
        assert_eq!(next_sequence(&mut resumed).await?, Some(2));
        assert_eq!(next_sequence(&mut resumed).await?, Some(3));
        handle.advance_tip(BlockHeight::from(4));
        assert_eq!(next_sequence(&mut resumed).await?, Some(4));

        handle.expire_chain_events_through(2);
        let expired = mock
            .chain_event_envelopes(ChainEventStreamStart::AfterCursor(
                ChainEventCursor::from_bytes(1_u64.to_be_bytes().to_vec()),
            ))
            .await;
        assert!(matches!(
            expired,
            Err(ChainSourceError::ChainEventCursorExpired { .. })
        ));
        let invalid = mock
            .chain_event_envelopes(ChainEventStreamStart::AfterCursor(
                ChainEventCursor::from_bytes(b"not a cursor".to_vec()),
            ))
            .await;
        assert!(matches!(
            invalid,
            Err(ChainSourceError::ChainEventCursorInvalid)
        ));
        let mut earliest = mock
            .chain_event_envelopes(ChainEventStreamStart::EarliestRetained)
            .await?;
        assert_eq!(next_sequence(&mut earliest).await?, Some(3));
        Ok(())
    }

    #[tokio::test]
    async fn a_reorg_replaces_hashes_and_mined_blocks_from_the_fork() -> Result<(), ChainSourceError>
    {
        let mock = MockChainSource::new(Network::regtest());
        let handle = mock.handle();
        handle.advance_tip(BlockHeight::from(10));
        let trees = MockCommitmentTrees {
            sapling_frontier_bytes: vec![1, 2, 3],
            orchard_frontier_bytes: vec![0, 0, 0],
            ironwood_frontier_bytes: vec![0, 0, 0],
            chain_metadata: CompactChainMetadata {
                sapling_commitment_tree_size: 1,
                orchard_commitment_tree_size: 0,
                ironwood_commitment_tree_size: 0,
            },
        };
        handle.mine_transactions(BlockHeight::from(8), Vec::new(), trees);
        let epoch = mock.current_epoch().await?;
        assert_eq!(
            mock.tree_state_at(epoch, BlockHeight::from(9))
                .await?
                .sapling_final_state_bytes,
            vec![1, 2, 3]
        );
        assert_eq!(
            handle.block_hash_at(BlockHeight::from(8)),
            BlockHash::from_bytes([0; 32])
        );

        handle.trigger_reorg(BlockHeight::from(7), BlockHeight::from(12));
        let epoch = mock.current_epoch().await?;
        assert_eq!(
            handle.block_hash_at(BlockHeight::from(6)),
            BlockHash::from_bytes([0; 32])
        );
        assert_ne!(
            handle.block_hash_at(BlockHeight::from(7)),
            BlockHash::from_bytes([0; 32])
        );
        let replacement = mock.tree_state_at(epoch, BlockHeight::from(9)).await?;
        assert_ne!(
            replacement.sapling_final_state_bytes,
            vec![1, 2, 3],
            "the abandoned block's commitment trees must not reach the replacement branch"
        );
        assert_eq!(
            replacement.block_hash,
            handle.block_hash_at(BlockHeight::from(9))
        );
        let range = BlockHeightRange::new(BlockHeight::from(7), BlockHeight::from(8))
            .ok_or(ChainSourceError::UnsupportedResponse { response: "range" })?;
        let mut blocks = mock.compact_blocks(epoch, range).await?;
        let first = _FuturesStreamExt::next(&mut blocks).await.transpose()?;
        assert_eq!(
            first.map(|block| block.previous_block_hash),
            Some(BlockHash::from_bytes([0; 32])),
            "the first replaced block chains onto the shared fork point"
        );
        Ok(())
    }
}
