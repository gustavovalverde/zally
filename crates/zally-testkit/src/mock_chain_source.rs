//! Programmable in-memory `ChainSource` fixture.
//!
//! `MockChainSource` lets a test:
//!
//! - set the visible chain tip via [`MockChainSourceHandle::advance_tip`],
//! - emit a [`ChainEvent::ChainReorged`] via [`MockChainSourceHandle::trigger_reorg`],
//! - serve scannable transactionless compact blocks by default, so scan progress commits,
//! - serve a caller-supplied tree state at its height via
//!   [`MockChainSourceHandle::serve_tree_state`], so tree-root verification sees
//!   chain roots the test controls.
//!
//! The handle returned by [`MockChainSource::handle`] is `Clone` and shares state with the
//! original mock; tests drive the mock through the handle while passing the mock itself to
//! `Wallet::sync`.

use std::collections::HashMap;
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
    ChainEventCursor, ChainEventEnvelope, ChainEventEnvelopeStream, ChainEventStreamStart,
    ChainRangeReverted, ChainSource, ChainSourceError, CompactBlockStream, ShieldedPool,
    SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};
use zally_core::{
    BlockHash, BlockHeight, CompactBlockArtifact, CompactChainMetadata, Network, TreeStateArtifact,
    TxId,
};
use zcash_protocol::consensus::{NetworkUpgrade, Parameters as _};

const EVENT_CHANNEL_CAPACITY: usize = 256;

struct MockState {
    network: Network,
    visible_tip_height: BlockHeight,
    settled_tip_height: BlockHeight,
    epoch_id: u64,
    current_epoch_failures: Vec<ChainSourceError>,
    transparent_utxo_failures: Vec<ChainSourceError>,
    subtree_failures: Vec<ChainSourceError>,
    compact_failures: Vec<ChainSourceError>,
    failures_consumed: u32,
    acquired_epoch_ids: Vec<u64>,
    artifact_epoch_ids: Vec<u64>,
    is_serving_compact_blocks: bool,
    tree_states_by_height: HashMap<u32, TreeStateArtifact>,
}

/// In-memory `ChainSource` fixture.
pub struct MockChainSource {
    state: Arc<Mutex<MockState>>,
    event_tx: broadcast::Sender<ChainEvent>,
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
                failures_consumed: 0,
                acquired_epoch_ids: Vec::new(),
                artifact_epoch_ids: Vec::new(),
                is_serving_compact_blocks: true,
                tree_states_by_height: HashMap::new(),
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
    event_tx: broadcast::Sender<ChainEvent>,
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
            let _ = self.event_tx.send(ChainEvent::ChainCommitted { committed });
        }
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

    /// Reverts to `reorg_from_height - 1` and announces a reorg that re-commits up to
    /// `new_settled_tip_height`.
    pub fn trigger_reorg(
        &self,
        reorg_from_height: BlockHeight,
        new_settled_tip_height: BlockHeight,
    ) {
        let (prior_tip, reverted_epoch);
        {
            let mut guard = self.state.lock();
            prior_tip = guard.settled_tip_height;
            reverted_epoch = chain_epoch_from_state(&guard);
            guard.visible_tip_height = new_settled_tip_height;
            guard.settled_tip_height = new_settled_tip_height;
            guard.epoch_id = guard.epoch_id.saturating_add(1);
        }
        let reverted = BlockHeightRange::new(reorg_from_height, prior_tip);
        let committed = BlockHeightRange::new(reorg_from_height, new_settled_tip_height);
        if let (Some(reverted_range), Some(committed_range)) = (reverted, committed) {
            let committed_epoch = chain_epoch_from_state(&self.state.lock());
            let _ = self.event_tx.send(ChainEvent::ChainReorged {
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
}

fn transactionless_compact_block(height: u32) -> CompactBlockArtifact {
    CompactBlockArtifact {
        height: BlockHeight::from(height),
        block_hash: BlockHash::from_bytes([0; 32]),
        previous_block_hash: BlockHash::from_bytes([0; 32]),
        block_time_seconds: height,
        transactions: Vec::new(),
        chain_metadata: CompactChainMetadata {
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        },
    }
}

fn chain_epoch_from_state(state: &MockState) -> ChainEpoch {
    ChainEpoch::new(
        ChainEpochId::new(state.epoch_id),
        state.network,
        BlockId {
            height: state.visible_tip_height,
            hash: BlockHash::from_bytes([0; 32]),
        },
        BlockId {
            height: state.settled_tip_height,
            hash: BlockHash::from_bytes([0; 32]),
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
        let visible_tip = BlockId {
            height: guard.visible_tip_height,
            hash: BlockHash::from_bytes([0; 32]),
        };
        let settled_tip = BlockId {
            height: guard.settled_tip_height,
            hash: BlockHash::from_bytes([0; 32]),
        };
        ChainEpoch::new(
            ChainEpochId::new(epoch_id),
            guard.network,
            visible_tip,
            settled_tip,
        )
        .ok_or(ChainSourceError::UnsupportedResponse {
            response: "MockChainEpoch",
        })
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
        let (tip, is_serving) = {
            let guard = self.state.lock();
            (guard.visible_tip_height, guard.is_serving_compact_blocks)
        };
        if block_range.end_height().as_u32() > tip.as_u32() {
            return Err(ChainSourceError::BlockHeightAboveVisibleTip {
                requested_height: block_range.end_height(),
                visible_tip_height: tip,
            });
        }
        let blocks: Vec<Result<CompactBlockArtifact, ChainSourceError>> = if is_serving {
            (block_range.start_height().as_u32()..=block_range.end_height().as_u32())
                .map(|height| Ok(transactionless_compact_block(height)))
                .collect()
        } else {
            Vec::new()
        };
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
        self.state
            .lock()
            .artifact_epoch_ids
            .push(chain_epoch.id().value());
        let (network, served_tree_state) = {
            self.validate_epoch(chain_epoch)?;
            let guard = self.state.lock();
            (
                guard.network,
                guard
                    .tree_states_by_height
                    .get(&block_height.as_u32())
                    .cloned(),
            )
        };
        if let Some(tree_state) = served_tree_state {
            return Ok(tree_state);
        }
        Ok(TreeStateArtifact {
            network,
            height: block_height,
            block_hash: BlockHash::from_bytes([0; 32]),
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

    async fn transaction_status(
        &self,
        _tx_id: TxId,
    ) -> Result<TransactionStatus, ChainSourceError> {
        Ok(TransactionStatus::NotFound)
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
        _start: ChainEventStreamStart,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
        let receiver = self.event_tx.subscribe();
        let stream = BroadcastStream::new(receiver).filter_map(|delivery| match delivery {
            Ok(event) => {
                let chain_epoch = match event_chain_epoch(&event) {
                    Ok(epoch) => epoch,
                    Err(error) => return Some(Err(error)),
                };
                Some(Ok(ChainEventEnvelope::new(
                    ChainEventCursor::from_bytes(chain_epoch.id().value().to_be_bytes().to_vec()),
                    chain_epoch.id().value(),
                    chain_epoch,
                    event,
                )))
            }
            Err(_lagged) => None,
        });
        Ok(Box::pin(stream))
    }
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
            .chain_event_envelopes(ChainEventStreamStart::EarliestRetained)
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
}
