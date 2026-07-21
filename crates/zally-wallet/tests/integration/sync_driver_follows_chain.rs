//! `SyncDriver` follows chain events without making callers write their own loop.

use std::future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::stream;
use parking_lot::Mutex;
use zally_chain::{
    BlockHeightRange, BlockId, ChainEpoch, ChainEpochCommitted, ChainEpochId, ChainEvent,
    ChainEventCursor, ChainEventCursorRecovery, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainEventStreamStart, ChainSource, ChainSourceError, CompactBlockStream, ShieldedPool,
    SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};
use zally_core::{BlockHash, BlockHeight, Network, TreeStateArtifact, TxId};
use zally_testkit::MockChainSource;
use zally_wallet::{
    SyncDriver, SyncDriverOptions, SyncDriverPhase, SyncRecoveryPolicy, SyncStatus, WalletError,
};

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
        snapshot.settled_tip_height == Some(BlockHeight::from(42))
    })
    .await?;

    assert_eq!(observed.settled_tip_height, Some(BlockHeight::from(42)));
    assert_eq!(
        observed.sync_status,
        SyncStatus::AtTip {
            visible_tip_height: BlockHeight::from(42)
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
async fn quiet_timer_ticks_do_not_repeat_wallet_sync() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let chain = Arc::new(QuietCountingChainSource::new(
        wallet.network(),
        false,
        false,
    ));
    let calls = Arc::clone(&chain.current_epoch_calls);
    let driver = SyncDriver::new(
        wallet,
        chain,
        SyncDriverOptions::default().with_poll_interval_ms(20),
    )?;
    let handle = driver.sync_continuously();
    tokio::time::sleep(Duration::from_millis(150)).await;
    handle.close().await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn expired_cursor_forces_sync_and_reopens_from_earliest() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let chain = Arc::new(QuietCountingChainSource::new(wallet.network(), true, false));
    let calls = Arc::clone(&chain.current_epoch_calls);
    let starts = Arc::clone(&chain.starts);
    let driver = SyncDriver::new(
        wallet,
        chain,
        SyncDriverOptions::default().with_poll_interval_ms(20),
    )?;
    let handle = driver.sync_continuously();
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) < 2 || starts.lock().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_elapsed| TestError::StartsTimedOut)?;
    handle.close().await?;
    assert_eq!(
        starts.lock().get(1),
        Some(&ChainEventStreamStart::EarliestRetained)
    );
    Ok(())
}

#[tokio::test]
async fn event_failure_retries_after_iteration_budget_without_second_event() -> Result<(), TestError>
{
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let chain = Arc::new(QuietCountingChainSource::new(wallet.network(), false, true));
    let chain_handle = chain.inner.handle();
    let calls = Arc::clone(&chain.current_epoch_calls);
    let driver = SyncDriver::new(
        wallet,
        chain,
        SyncDriverOptions::default()
            .with_poll_interval_ms(60_000)
            .with_max_sync_iterations_per_wake_count(1)
            .with_recovery_policy(
                SyncRecoveryPolicy::default()
                    .with_fault_backoff_initial_ms(1)
                    .with_fault_backoff_cap_ms(1),
            ),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
    })
    .await?;
    chain_handle.fail_current_epoch_next(1, || ChainSourceError::Unavailable {
        reason: "injected event-triggered sync failure".to_owned(),
    });
    chain_handle.advance_tip(BlockHeight::from(42));
    tokio::time::timeout(Duration::from_secs(5), async {
        while calls.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_elapsed| TestError::StartsTimedOut)?;
    handle.close().await?;
    assert!(calls.load(Ordering::SeqCst) >= 3);
    assert_eq!(chain_handle.failures_consumed(), 1);
    Ok(())
}

struct QuietCountingChainSource {
    inner: MockChainSource,
    current_epoch_calls: Arc<AtomicUsize>,
    starts: Arc<Mutex<Vec<ChainEventStreamStart>>>,
    expire_first_stream: bool,
    delegate_events: bool,
}

impl QuietCountingChainSource {
    fn new(network: Network, expire_first_stream: bool, delegate_events: bool) -> Self {
        Self {
            inner: MockChainSource::new(network),
            current_epoch_calls: Arc::new(AtomicUsize::new(0)),
            starts: Arc::new(Mutex::new(Vec::new())),
            expire_first_stream,
            delegate_events,
        }
    }
}

#[async_trait::async_trait]
impl ChainSource for QuietCountingChainSource {
    fn network(&self) -> Network {
        self.inner.network()
    }

    async fn current_epoch(&self) -> Result<ChainEpoch, ChainSourceError> {
        self.current_epoch_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.current_epoch().await
    }

    async fn compact_blocks(
        &self,
        epoch: ChainEpoch,
        range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        self.inner.compact_blocks(epoch, range).await
    }

    async fn tree_state_at(
        &self,
        epoch: ChainEpoch,
        height: BlockHeight,
    ) -> Result<TreeStateArtifact, ChainSourceError> {
        self.inner.tree_state_at(epoch, height).await
    }

    async fn subtree_roots(
        &self,
        epoch: ChainEpoch,
        pool: ShieldedPool,
        start: SubtreeIndex,
        count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
        self.inner.subtree_roots(epoch, pool, start, count).await
    }

    async fn transaction_status(&self, tx_id: TxId) -> Result<TransactionStatus, ChainSourceError> {
        self.inner.transaction_status(tx_id).await
    }

    async fn transparent_utxos(
        &self,
        epoch: ChainEpoch,
        script: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        self.inner.transparent_utxos(epoch, script).await
    }

    async fn chain_event_envelopes(
        &self,
        start: ChainEventStreamStart,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
        let delegated_start = start.clone();
        let index = {
            let mut starts = self.starts.lock();
            let index = starts.len();
            starts.push(start);
            index
        };
        if self.expire_first_stream && index == 0 {
            Ok(Box::pin(stream::once(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err(ChainSourceError::ChainEventCursorExpired {
                    recovery: ChainEventCursorRecovery::EarliestRetained,
                })
            })))
        } else if self.delegate_events {
            self.inner.chain_event_envelopes(delegated_start).await
        } else {
            Ok(Box::pin(stream::pending()))
        }
    }
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
    let tip = BlockId {
        height: BlockHeight::from(7),
        hash: BlockHash::from_bytes([7; 32]),
    };
    let chain_epoch = ChainEpoch::new(ChainEpochId::new(7), network, tip, tip)
        .ok_or(TestError::InvalidFixtureEpoch)?;
    let delivered = ChainEventEnvelope::new(
        delivered_cursor.clone(),
        7,
        chain_epoch,
        ChainEvent::ChainCommitted {
            committed: ChainEpochCommitted {
                chain_epoch,
                block_range: committed_range,
            },
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

    async fn current_epoch(&self) -> Result<ChainEpoch, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn compact_blocks(
        &self,
        _chain_epoch: ChainEpoch,
        _block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn tree_state_at(
        &self,
        _chain_epoch: ChainEpoch,
        _block_height: BlockHeight,
    ) -> Result<TreeStateArtifact, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn subtree_roots(
        &self,
        _chain_epoch: ChainEpoch,
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
        _chain_epoch: ChainEpoch,
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

    async fn current_epoch(&self) -> Result<ChainEpoch, ChainSourceError> {
        self.inner.current_epoch().await
    }

    async fn compact_blocks(
        &self,
        chain_epoch: ChainEpoch,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        self.inner.compact_blocks(chain_epoch, block_range).await
    }

    async fn tree_state_at(
        &self,
        chain_epoch: ChainEpoch,
        block_height: BlockHeight,
    ) -> Result<TreeStateArtifact, ChainSourceError> {
        self.inner.tree_state_at(chain_epoch, block_height).await
    }

    async fn subtree_roots(
        &self,
        chain_epoch: ChainEpoch,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
        self.inner
            .subtree_roots(chain_epoch, pool, start_index, max_count)
            .await
    }

    async fn transaction_status(&self, tx_id: TxId) -> Result<TransactionStatus, ChainSourceError> {
        self.inner.transaction_status(tx_id).await
    }

    async fn transparent_utxos(
        &self,
        chain_epoch: ChainEpoch,
        script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        self.inner
            .transparent_utxos(chain_epoch, script_pub_key_bytes)
            .await
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
    #[error("fixture produced an invalid chain epoch")]
    InvalidFixtureEpoch,
    #[error("timed out waiting for the driver to record subscription starts")]
    StartsTimedOut,
}
