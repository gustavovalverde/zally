//! Programmable in-memory `ChainSource` fixture.
//!
//! `MockChainSource` lets a test:
//!
//! - set the visible chain tip via [`MockChainSourceHandle::advance_tip`],
//! - emit a [`ChainEvent::ChainReorged`] via [`MockChainSourceHandle::trigger_reorg`],
//! - stream empty compact blocks (the fixture covers sync orchestration; tests that need
//!   real note decryption point `Wallet::sync` at a live `ChainSource` instead),
//! - serve scannable transactionless compact blocks via
//!   [`MockChainSourceHandle::serve_compact_blocks`], so scan progress commits,
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
    BlockHeightRange, ChainEvent, ChainEventCursor, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainEventStreamStart, ChainSource, ChainSourceError, CompactBlockStream, ShieldedPool,
    SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};
use zally_core::{BlockHeight, Network, TxId};
use zcash_client_backend::proto::compact_formats::{ChainMetadata, CompactBlock};
use zcash_client_backend::proto::service::TreeState;

const EVENT_CHANNEL_CAPACITY: usize = 256;

struct MockState {
    network: Network,
    tip_height: BlockHeight,
    chain_tip_failures: Vec<ChainSourceError>,
    transparent_utxo_failures: Vec<ChainSourceError>,
    failures_consumed: u32,
    is_serving_compact_blocks: bool,
    tree_states_by_height: HashMap<u32, TreeState>,
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
                tip_height: BlockHeight::GENESIS,
                chain_tip_failures: Vec::new(),
                transparent_utxo_failures: Vec::new(),
                failures_consumed: 0,
                is_serving_compact_blocks: false,
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
}

/// Programmable handle for driving a [`MockChainSource`] from a test.
#[derive(Clone)]
pub struct MockChainSourceHandle {
    state: Arc<Mutex<MockState>>,
    event_tx: broadcast::Sender<ChainEvent>,
}

impl MockChainSourceHandle {
    /// Sets the visible chain tip to `new_safe_chain_tip_height` and emits `SafeChainTipAdvanced`.
    pub fn advance_tip(&self, new_safe_chain_tip_height: BlockHeight) {
        let committed_range = {
            let mut guard = self.state.lock();
            let prior_tip = guard.tip_height;
            guard.tip_height = new_safe_chain_tip_height;
            drop(guard);
            BlockHeightRange::new(
                BlockHeight::from(prior_tip.as_u32().saturating_add(1)),
                new_safe_chain_tip_height,
            )
        };
        if let Some(range) = committed_range {
            let _ = self.event_tx.send(ChainEvent::SafeChainTipAdvanced {
                committed_range: range,
                new_safe_chain_tip_height,
            });
        }
    }

    /// Reverts to `reorg_from_height - 1` and announces a reorg that re-commits up to
    /// `new_safe_chain_tip_height`.
    pub fn trigger_reorg(
        &self,
        reorg_from_height: BlockHeight,
        new_safe_chain_tip_height: BlockHeight,
    ) {
        let prior_tip;
        {
            let mut guard = self.state.lock();
            prior_tip = guard.tip_height;
            guard.tip_height = new_safe_chain_tip_height;
        }
        let reverted = BlockHeightRange::new(reorg_from_height, prior_tip);
        let committed = BlockHeightRange::new(reorg_from_height, new_safe_chain_tip_height);
        if let (Some(reverted_range), Some(committed_range)) = (reverted, committed) {
            let _ = self.event_tx.send(ChainEvent::ChainReorged {
                reverted_range,
                committed_range,
                new_safe_chain_tip_height,
            });
        }
    }

    /// Current visible chain tip.
    #[must_use]
    pub fn current_tip(&self) -> BlockHeight {
        self.state.lock().tip_height
    }

    /// Queues `count` consecutive failures for `chain_tip` calls. Each subsequent call pops
    /// one failure off the queue; once empty, calls succeed normally. Failures are returned
    /// in the order they were queued.
    ///
    /// `produce_error` is invoked once per queued failure, so callers can either return the
    /// same error each time (`|| ChainSourceError::Unavailable { ... }`) or vary the error
    /// per attempt. The closure avoids requiring `Clone` on the error type, which would
    /// otherwise force [`ChainSourceError::Indexer`] to carry an `Arc<IndexerError>`.
    pub fn fail_chain_tip_next(
        &self,
        count: u32,
        mut produce_error: impl FnMut() -> ChainSourceError,
    ) {
        let mut guard = self.state.lock();
        for _ in 0..count {
            guard.chain_tip_failures.push(produce_error());
        }
    }

    /// Queues `count` consecutive failures for `transparent_utxos` calls. Mirrors
    /// [`Self::fail_chain_tip_next`]: each call pops one failure off the queue; once empty,
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
    pub fn serve_tree_state(&self, tree_state: TreeState) {
        let height = u32::try_from(tree_state.height).unwrap_or(u32::MAX);
        self.state
            .lock()
            .tree_states_by_height
            .insert(height, tree_state);
    }

    /// Number of failures consumed since the mock was constructed.
    #[must_use]
    pub fn failures_consumed(&self) -> u32 {
        self.state.lock().failures_consumed
    }
}

fn transactionless_compact_block(height: u32) -> CompactBlock {
    CompactBlock {
        height: u64::from(height),
        hash: vec![0; 32],
        prev_hash: vec![0; 32],
        time: height,
        chain_metadata: Some(ChainMetadata::default()),
        ..CompactBlock::default()
    }
}

#[async_trait]
impl ChainSource for MockChainSource {
    fn network(&self) -> Network {
        self.state.lock().network
    }

    async fn safe_chain_tip(&self) -> Result<BlockHeight, ChainSourceError> {
        Ok(self.state.lock().tip_height)
    }

    async fn chain_tip(&self) -> Result<BlockHeight, ChainSourceError> {
        let mut guard = self.state.lock();
        if !guard.chain_tip_failures.is_empty() {
            let injected = guard.chain_tip_failures.remove(0);
            guard.failures_consumed = guard.failures_consumed.saturating_add(1);
            return Err(injected);
        }
        Ok(guard.tip_height)
    }

    async fn compact_blocks(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        let (tip, is_serving) = {
            let guard = self.state.lock();
            (guard.tip_height, guard.is_serving_compact_blocks)
        };
        if block_range.end_height.as_u32() > tip.as_u32() {
            return Err(ChainSourceError::BlockHeightAboveSafeChainTip {
                requested_height: block_range.end_height,
                safe_chain_tip_height: tip,
            });
        }
        // The default empty stream covers sync orchestration. Tests that need the scan loop
        // opt in via `serve_compact_blocks`; note decryption still needs a live `ChainSource`.
        let blocks: Vec<Result<CompactBlock, ChainSourceError>> = if is_serving {
            (block_range.start_height.as_u32()..=block_range.end_height.as_u32())
                .map(|height| Ok(transactionless_compact_block(height)))
                .collect()
        } else {
            Vec::new()
        };
        let stream: Pin<Box<dyn Stream<Item = Result<CompactBlock, ChainSourceError>> + Send>> =
            Box::pin(tokio_stream::iter(blocks));
        Ok(stream)
    }

    async fn tree_state_at(
        &self,
        block_height: BlockHeight,
    ) -> Result<TreeState, ChainSourceError> {
        let (network, served_tree_state) = {
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
        Ok(TreeState {
            network: format!("{network:?}"),
            height: u64::from(block_height.as_u32()),
            hash: "00".repeat(32),
            time: 0,
            sapling_tree: String::new(),
            orchard_tree: String::new(),
            ironwood_tree: String::new(),
        })
    }

    async fn subtree_roots(
        &self,
        _pool: ShieldedPool,
        _start_index: SubtreeIndex,
        _max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
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
        _script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
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
                let new_safe_chain_tip_height = event_tip_height(&event);
                Some(Ok(ChainEventEnvelope::new(
                    ChainEventCursor::from_bytes(
                        new_safe_chain_tip_height.as_u32().to_be_bytes().to_vec(),
                    ),
                    u64::from(new_safe_chain_tip_height.as_u32()),
                    new_safe_chain_tip_height,
                    event,
                )))
            }
            Err(_lagged) => None,
        });
        Ok(Box::pin(stream))
    }
}

fn event_tip_height(event: &ChainEvent) -> BlockHeight {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "non_exhaustive chain events map unknown variants to the genesis cursor"
    )]
    match event {
        ChainEvent::SafeChainTipAdvanced {
            new_safe_chain_tip_height,
            ..
        }
        | ChainEvent::ChainReorged {
            new_safe_chain_tip_height,
            ..
        } => *new_safe_chain_tip_height,
        _ => BlockHeight::GENESIS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _FuturesStreamExt;

    #[tokio::test]
    async fn mock_chain_tip_advances_on_handle_call() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest());
        let handle = mock.handle();
        assert_eq!(mock.safe_chain_tip().await?, BlockHeight::GENESIS);
        handle.advance_tip(BlockHeight::from(10));
        assert_eq!(mock.safe_chain_tip().await?, BlockHeight::from(10));
        Ok(())
    }

    #[tokio::test]
    async fn mock_emits_chain_tip_advanced_event() -> Result<(), ChainSourceError> {
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
                ChainEvent::SafeChainTipAdvanced { new_safe_chain_tip_height, .. } if new_safe_chain_tip_height == BlockHeight::from(3)
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
                ChainEvent::ChainReorged { new_safe_chain_tip_height, .. } if new_safe_chain_tip_height == BlockHeight::from(11)
            )
        ));
        Ok(())
    }

    #[tokio::test]
    async fn mock_compact_blocks_returns_empty() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest());
        mock.handle().advance_tip(BlockHeight::from(5));
        let range =
            BlockHeightRange::new(BlockHeight::from(1), BlockHeight::from(5)).ok_or_else(|| {
                ChainSourceError::Unavailable {
                    reason: "range".into(),
                }
            })?;
        let mut stream = mock.compact_blocks(range).await?;
        assert!(_FuturesStreamExt::next(&mut stream).await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn mock_compact_blocks_above_tip_rejected() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest());
        let range = BlockHeightRange::new(BlockHeight::from(1), BlockHeight::from(100))
            .ok_or_else(|| ChainSourceError::Unavailable {
                reason: "range".into(),
            })?;
        let outcome = mock.compact_blocks(range).await;
        assert!(matches!(
            outcome,
            Err(ChainSourceError::BlockHeightAboveSafeChainTip { .. })
        ));
        Ok(())
    }
}
