//! `SyncDriver` follows chain events without making callers write their own loop.

use std::future;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream;
use parking_lot::Mutex;
use zally_chain::{
    BlockHeightRange, ChainEvent, ChainEventCursor, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainEventStreamStart, ChainSource, ChainSourceError, CompactBlockStream, ShieldedPool,
    SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};
use zally_core::{BlockHeight, Network, TxId};
use zally_testkit::MockChainSource;
use zally_wallet::{SyncDriver, SyncDriverOptions, SyncDriverPhase, SyncStatus, WalletError};
use zcash_client_backend::proto::service::TreeState;

use super::fixtures::{TestWalletFixture, create_test_wallet, wait_for_snapshot};

#[tokio::test]
async fn sync_driver_wakes_from_chain_event() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    let chain_source: Arc<dyn ChainSource> = chain;
    let driver = SyncDriver::new(
        wallet,
        chain_source,
        SyncDriverOptions::default()
            .with_poll_interval_ms(60_000)
            .with_max_sync_iterations_per_wake_count(4),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
    })
    .await?;
    chain_handle.advance_tip(BlockHeight::from(42));
    let observed = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.safe_chain_tip_height == Some(BlockHeight::from(42))
    })
    .await?;

    assert_eq!(observed.safe_chain_tip_height, Some(BlockHeight::from(42)));
    assert_eq!(
        observed.sync_status,
        SyncStatus::Starting {
            target_height: BlockHeight::from(42)
        }
    );
    assert_eq!(observed.last_fault, None);

    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn close_returns_while_sync_attempt_is_blocked() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let chain_source: Arc<dyn ChainSource> = Arc::new(StalledChainSource::new(network));
    let driver = SyncDriver::new(
        wallet,
        chain_source,
        SyncDriverOptions::default()
            .with_poll_interval_ms(60_000)
            .with_sync_timeout_seconds(60),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.phase == SyncDriverPhase::Syncing
    })
    .await?;
    tokio::time::timeout(Duration::from_millis(250), handle.close())
        .await
        .map_err(|_elapsed| TestError::CloseTimedOut)??;

    Ok(())
}

#[tokio::test]
async fn event_stream_reopens_after_the_last_delivered_cursor() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let delivered_cursor = ChainEventCursor::from_bytes(7_u32.to_be_bytes().to_vec());
    let committed_range = BlockHeightRange::new(BlockHeight::from(1), BlockHeight::from(7))
        .ok_or(TestError::InvalidFixtureRange)?;
    let delivered = ChainEventEnvelope::new(
        delivered_cursor.clone(),
        7,
        BlockHeight::from(7),
        ChainEvent::SafeChainTipAdvanced {
            committed_range,
            new_safe_chain_tip_height: BlockHeight::from(7),
        },
    );

    let chain = Arc::new(RecordingChainSource::new(network, delivered));
    let recorded_starts = chain.starts();
    let chain_source: Arc<dyn ChainSource> = chain;
    let driver = SyncDriver::new(
        wallet,
        chain_source,
        SyncDriverOptions::default().with_poll_interval_ms(25),
    )?;
    let handle = driver.sync_continuously();

    let starts = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let guard = recorded_starts.lock();
                if guard.len() >= 2 {
                    return guard.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_elapsed| TestError::StartsTimedOut)?;

    handle.close().await?;

    assert_eq!(
        starts.first(),
        Some(&ChainEventStreamStart::EarliestRetained),
        "the first subscription must bootstrap from the earliest retained event"
    );
    assert_eq!(
        starts.get(1),
        Some(&ChainEventStreamStart::AfterCursor(delivered_cursor)),
        "the reopen after a stream fault must resume strictly after the delivered cursor"
    );
    Ok(())
}

struct StalledChainSource {
    network: Network,
}

impl StalledChainSource {
    const fn new(network: Network) -> Self {
        Self { network }
    }
}

#[async_trait::async_trait]
impl ChainSource for StalledChainSource {
    fn network(&self) -> Network {
        self.network
    }

    async fn safe_chain_tip(&self) -> Result<BlockHeight, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn chain_tip(&self) -> Result<BlockHeight, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn compact_blocks(
        &self,
        _block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn tree_state_at(
        &self,
        _block_height: BlockHeight,
    ) -> Result<TreeState, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn subtree_roots(
        &self,
        _pool: ShieldedPool,
        _start_index: SubtreeIndex,
        _max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn transaction_status(
        &self,
        _tx_id: TxId,
    ) -> Result<TransactionStatus, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn transparent_utxos(
        &self,
        _script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn chain_event_envelopes(
        &self,
        _start: ChainEventStreamStart,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
        let _ = self.network;
        tokio::task::yield_now().await;
        Ok(Box::pin(stream::pending::<
            Result<ChainEventEnvelope, ChainSourceError>,
        >()))
    }
}

/// `ChainSource` that records every subscription start and scripts the event stream.
///
/// The first subscription delivers one envelope and then faults, forcing the driver to
/// resubscribe; every later subscription stays open. Recording the start argument makes the
/// driver's resume contract observable: bootstrap from
/// [`ChainEventStreamStart::EarliestRetained`], then reopen from
/// [`ChainEventStreamStart::AfterCursor`] once an event has been delivered.
struct RecordingChainSource {
    inner: MockChainSource,
    delivered: ChainEventEnvelope,
    starts: Arc<Mutex<Vec<ChainEventStreamStart>>>,
}

impl RecordingChainSource {
    fn new(network: Network, delivered: ChainEventEnvelope) -> Self {
        Self {
            inner: MockChainSource::new(network),
            delivered,
            starts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn starts(&self) -> Arc<Mutex<Vec<ChainEventStreamStart>>> {
        Arc::clone(&self.starts)
    }
}

#[async_trait::async_trait]
impl ChainSource for RecordingChainSource {
    fn network(&self) -> Network {
        self.inner.network()
    }

    async fn safe_chain_tip(&self) -> Result<BlockHeight, ChainSourceError> {
        self.inner.safe_chain_tip().await
    }

    async fn chain_tip(&self) -> Result<BlockHeight, ChainSourceError> {
        self.inner.chain_tip().await
    }

    async fn compact_blocks(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        self.inner.compact_blocks(block_range).await
    }

    async fn tree_state_at(
        &self,
        block_height: BlockHeight,
    ) -> Result<TreeState, ChainSourceError> {
        self.inner.tree_state_at(block_height).await
    }

    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
        self.inner.subtree_roots(pool, start_index, max_count).await
    }

    async fn transaction_status(&self, tx_id: TxId) -> Result<TransactionStatus, ChainSourceError> {
        self.inner.transaction_status(tx_id).await
    }

    async fn transparent_utxos(
        &self,
        script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        self.inner.transparent_utxos(script_pub_key_bytes).await
    }

    async fn chain_event_envelopes(
        &self,
        start: ChainEventStreamStart,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
        let subscription_index = {
            let mut guard = self.starts.lock();
            let index = guard.len();
            guard.push(start);
            index
        };
        if subscription_index == 0 {
            let scripted: Vec<Result<ChainEventEnvelope, ChainSourceError>> = vec![
                Ok(self.delivered.clone()),
                Err(ChainSourceError::Unavailable {
                    reason: "synthetic stream fault".into(),
                }),
            ];
            Ok(Box::pin(stream::iter(scripted)))
        } else {
            Ok(Box::pin(stream::pending::<
                Result<ChainEventEnvelope, ChainSourceError>,
            >()))
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] super::fixtures::TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("snapshot wait failed: {0}")]
    SnapshotWait(#[from] super::fixtures::SnapshotWaitError),
    #[error("timed out waiting for sync driver close")]
    CloseTimedOut,
    #[error("fixture produced an invalid block-height range")]
    InvalidFixtureRange,
    #[error("timed out waiting for the driver to record subscription starts")]
    StartsTimedOut,
}
