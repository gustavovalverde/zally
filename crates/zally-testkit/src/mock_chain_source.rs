//! Programmable in-memory `ChainSource` fixture.
//!
//! Used by Slice 2 integration tests. `MockChainSource` lets a test:
//!
//! - set the visible chain tip via [`MockChainSourceHandle::advance_tip`],
//! - emit a [`ChainEvent::ChainReorged`] via [`MockChainSourceHandle::trigger_reorg`],
//! - stream empty compact blocks (real scanning lands when Slice 5 wires live data).
//!
//! The handle returned by [`MockChainSource::handle`] is `Clone` and shares state with the
//! original mock; tests drive the mock through the handle while passing the mock itself to
//! `Wallet::sync`.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::Stream;
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use zally_chain::{
    BlockHeightRange, ChainEvent, ChainEventStream, ChainSource, ChainSourceError,
    CompactBlockStream, ShieldedPool, SubtreeIndex, SubtreeRoot, TransactionStatus,
    TransparentUtxo,
};
use zally_core::{BlockHeight, Network, TxId};
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_client_backend::proto::service::TreeState;

const EVENT_CHANNEL_CAPACITY: usize = 256;

struct MockState {
    network: Network,
    tip_height: BlockHeight,
    finalized_height: Option<BlockHeight>,
    chain_tip_failures: Vec<ChainSourceError>,
    failures_consumed: u32,
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
                finalized_height: None,
                chain_tip_failures: Vec::new(),
                failures_consumed: 0,
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
    /// Sets the visible chain tip to `new_tip_height` and emits `ChainTipAdvanced`.
    pub fn advance_tip(&self, new_tip_height: BlockHeight) {
        let committed_range = {
            let mut guard = self.state.lock();
            let prior_tip = guard.tip_height;
            guard.tip_height = new_tip_height;
            drop(guard);
            BlockHeightRange::new(
                BlockHeight::from(prior_tip.as_u32().saturating_add(1)),
                new_tip_height,
            )
        };
        if let Some(range) = committed_range {
            let _ = self.event_tx.send(ChainEvent::ChainTipAdvanced {
                committed_range: range,
                new_tip_height,
            });
        }
    }

    /// Reverts to `reorg_from_height - 1` and announces a reorg that re-commits up to
    /// `new_tip_height`.
    pub fn trigger_reorg(&self, reorg_from_height: BlockHeight, new_tip_height: BlockHeight) {
        let prior_tip;
        {
            let mut guard = self.state.lock();
            prior_tip = guard.tip_height;
            guard.tip_height = new_tip_height;
        }
        let reverted = BlockHeightRange::new(reorg_from_height, prior_tip);
        let committed = BlockHeightRange::new(reorg_from_height, new_tip_height);
        if let (Some(reverted_range), Some(committed_range)) = (reverted, committed) {
            let _ = self.event_tx.send(ChainEvent::ChainReorged {
                reverted_range,
                committed_range,
                new_tip_height,
            });
        }
    }

    /// Current visible chain tip.
    #[must_use]
    pub fn current_tip(&self) -> BlockHeight {
        self.state.lock().tip_height
    }

    /// Sets the finalized height the mock reports from `ChainSource::finalized_height`.
    /// When unset, the mock reports its visible tip as finalized (every block treated as
    /// final), matching the trait's default behavior.
    pub fn set_finalized_height(&self, finalized_height: BlockHeight) {
        self.state.lock().finalized_height = Some(finalized_height);
    }

    /// Queues `count` consecutive failures for `chain_tip` calls. Each subsequent call pops
    /// one failure off the queue; once empty, calls succeed normally. Failures are returned
    /// in the order they were queued.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "test API: taking the error template by value reads more naturally at the \
                  call site than threading a borrow through the closure"
    )]
    pub fn fail_chain_tip_next(&self, count: u32, error: ChainSourceError) {
        let mut guard = self.state.lock();
        for _ in 0..count {
            guard.chain_tip_failures.push(error.clone());
        }
    }

    /// Number of failures consumed since the mock was constructed.
    #[must_use]
    pub fn failures_consumed(&self) -> u32 {
        self.state.lock().failures_consumed
    }
}

#[async_trait]
impl ChainSource for MockChainSource {
    fn network(&self) -> Network {
        self.state.lock().network
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

    async fn finalized_height(&self) -> Result<BlockHeight, ChainSourceError> {
        let guard = self.state.lock();
        Ok(guard.finalized_height.unwrap_or(guard.tip_height))
    }

    async fn compact_blocks(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        let tip = self.state.lock().tip_height;
        if block_range.end_height.as_u32() > tip.as_u32() {
            return Err(ChainSourceError::BlockHeightAboveTip {
                requested_height: block_range.end_height,
                tip_height: tip,
            });
        }
        // Slice 2 mock returns an empty stream: tests cover sync orchestration, not the
        // scan loop's note decryption (that lands in Slice 5).
        let empty: Pin<Box<dyn Stream<Item = Result<CompactBlock, ChainSourceError>> + Send>> =
            Box::pin(tokio_stream::iter(Vec::<
                Result<CompactBlock, ChainSourceError>,
            >::new()));
        Ok(empty)
    }

    async fn tree_state_at(
        &self,
        block_height: BlockHeight,
    ) -> Result<TreeState, ChainSourceError> {
        Ok(TreeState {
            network: format!("{:?}", self.state.lock().network),
            height: u64::from(block_height.as_u32()),
            hash: "00".repeat(32),
            time: 0,
            sapling_tree: String::new(),
            orchard_tree: String::new(),
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
        _script_pub_key: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        Ok(Vec::new())
    }

    async fn chain_events(&self) -> Result<ChainEventStream, ChainSourceError> {
        let receiver = self.event_tx.subscribe();
        let stream = BroadcastStream::new(receiver).filter_map(|delivery| match delivery {
            Ok(event) => Some(Ok(event)),
            Err(_lagged) => None,
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _FuturesStreamExt;

    #[tokio::test]
    async fn mock_chain_tip_advances_on_handle_call() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest_all_at_genesis());
        let handle = mock.handle();
        assert_eq!(mock.chain_tip().await?, BlockHeight::GENESIS);
        handle.advance_tip(BlockHeight::from(10));
        assert_eq!(mock.chain_tip().await?, BlockHeight::from(10));
        Ok(())
    }

    #[tokio::test]
    async fn mock_emits_chain_tip_advanced_event() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest_all_at_genesis());
        let mut events = mock.chain_events().await?;
        let handle = mock.handle();
        handle.advance_tip(BlockHeight::from(3));
        let first = _FuturesStreamExt::next(&mut events).await;
        assert!(matches!(
            first,
            Some(Ok(ChainEvent::ChainTipAdvanced { new_tip_height, .. })) if new_tip_height == BlockHeight::from(3)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn mock_emits_chain_reorged_event() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest_all_at_genesis());
        let handle = mock.handle();
        handle.advance_tip(BlockHeight::from(10));
        let mut events = mock.chain_events().await?;
        handle.trigger_reorg(BlockHeight::from(7), BlockHeight::from(11));
        let event = _FuturesStreamExt::next(&mut events).await;
        assert!(matches!(
            event,
            Some(Ok(ChainEvent::ChainReorged { new_tip_height, .. })) if new_tip_height == BlockHeight::from(11)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn mock_compact_blocks_returns_empty() -> Result<(), ChainSourceError> {
        let mock = MockChainSource::new(Network::regtest_all_at_genesis());
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
        let mock = MockChainSource::new(Network::regtest_all_at_genesis());
        let range = BlockHeightRange::new(BlockHeight::from(1), BlockHeight::from(100))
            .ok_or_else(|| ChainSourceError::Unavailable {
                reason: "range".into(),
            })?;
        let outcome = mock.compact_blocks(range).await;
        assert!(matches!(
            outcome,
            Err(ChainSourceError::BlockHeightAboveTip { .. })
        ));
        Ok(())
    }
}
