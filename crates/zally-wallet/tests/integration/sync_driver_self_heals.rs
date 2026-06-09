//! The sync driver self-heals: faults engage an escalating repair ladder instead of
//! killing the task, dead ends park and reprobe, and the snapshot stream never ends while
//! the handle is alive.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use zally_chain::{
    BlockHeightRange, ChainEventCursor, ChainEventEnvelopeStream, ChainSource, ChainSourceError,
    CompactBlockStream, ShieldedPool, SubtreeIndex, SubtreeRoot, TransactionStatus,
    TransparentUtxo,
};
use zally_core::{BlockHeight, Network, TxId};
use zally_testkit::MockChainSource;
use zally_wallet::{
    SyncDriver, SyncDriverOptions, SyncDriverPhase, SyncRecoveryPolicy, SyncRepair, SyncSnapshot,
    SyncSnapshotStream, WalletError, WalletEvent,
};
use zcash_client_backend::proto::service::TreeState;

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

fn fast_recovery_policy() -> SyncRecoveryPolicy {
    SyncRecoveryPolicy::default()
        .with_escalate_after_faults(1)
        .with_max_rescan_attempts(2)
        .with_fault_backoff_initial_ms(20)
        .with_fault_backoff_cap_ms(40)
        .with_park_reprobe_ms(None)
}

#[tokio::test]
async fn fault_ladder_escalates_through_rewind_to_rescan_then_recovers() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let mut wallet_events = wallet.observe();
    let network = wallet.network();

    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    chain_handle.advance_tip(BlockHeight::from(40));
    chain_handle.fail_chain_tip_next(3, || ChainSourceError::MalformedCompactBlock {
        block_height: BlockHeight::from(10),
        reason: "synthetic derived-state corruption".into(),
    });

    let chain_source: Arc<dyn ChainSource> = chain;
    let driver = SyncDriver::new(
        wallet,
        chain_source,
        SyncDriverOptions::default()
            .with_poll_interval_ms(25)
            .with_recovery_policy(fast_recovery_policy()),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    let mut saw_rewind = false;
    let mut saw_rescan = false;
    let healthy = wait_for_snapshot(&mut snapshots, |snapshot| {
        if matches!(
            snapshot.phase,
            SyncDriverPhase::Recovering {
                repair: SyncRepair::Rewind,
                ..
            }
        ) {
            saw_rewind = true;
        }
        if matches!(
            snapshot.phase,
            SyncDriverPhase::Recovering {
                repair: SyncRepair::RescanFromBirthday,
                ..
            }
        ) {
            saw_rescan = true;
        }
        saw_rescan && snapshot.phase == SyncDriverPhase::Waiting && snapshot.last_fault.is_none()
    })
    .await?;

    assert!(saw_rewind, "the ladder must pass through the rewind rung");
    assert!(saw_rescan, "the ladder must reach the rescan rung");
    assert_eq!(healthy.last_fault, None);
    assert!(healthy.last_outcome.is_some());
    assert_eq!(chain_handle.failures_consumed(), 3);

    let reset_observed = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = wallet_events.next().await {
            if matches!(event, WalletEvent::DerivedStateReset { .. }) {
                return true;
            }
        }
        false
    })
    .await
    .map_err(|_| TestError::EventTimeout)?;
    assert!(
        reset_observed,
        "the rescan rung must rebuild derived state from the birthday"
    );

    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn park_reprobes_and_rearms_the_ladder() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let chain = Arc::new(ShiftingNetworkChain::new(network));
    let driver = SyncDriver::new(
        wallet,
        Arc::clone(&chain) as Arc<dyn ChainSource>,
        SyncDriverOptions::default()
            .with_poll_interval_ms(20)
            .with_recovery_policy(
                fast_recovery_policy()
                    .with_fault_backoff_initial_ms(1)
                    .with_fault_backoff_cap_ms(2)
                    .with_park_reprobe_ms(Some(60)),
            ),
    )?;
    chain.report_network(Network::Mainnet);
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    let first_parked = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, SyncDriverPhase::Parked { .. })
    })
    .await?;
    let SyncDriverPhase::Parked {
        since_ms: first_since_ms,
        reprobe_at_ms,
    } = first_parked.phase
    else {
        return Err(TestError::UnexpectedPhase);
    };
    assert!(reprobe_at_ms.is_some());
    assert!(
        first_parked
            .last_fault
            .as_ref()
            .is_some_and(|fault| fault.reason.contains("network mismatch")),
        "the parked snapshot must keep republishing its reason"
    );

    let refreshed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, SyncDriverPhase::Parked { .. })
            && snapshot.published_at_ms > first_parked.published_at_ms
    })
    .await?;
    assert!(refreshed.published_at_ms > first_parked.published_at_ms);

    wait_for_snapshot(&mut snapshots, |snapshot| {
        !matches!(snapshot.phase, SyncDriverPhase::Parked { .. })
    })
    .await?;

    let reparked = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(
            snapshot.phase,
            SyncDriverPhase::Parked { since_ms, .. } if since_ms > first_since_ms
        )
    })
    .await?;
    assert!(reparked.last_fault.is_some());

    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn status_stream_keeps_yielding_while_parked_until_close() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let chain = Arc::new(ShiftingNetworkChain::new(network));
    let driver = SyncDriver::new(
        wallet,
        Arc::clone(&chain) as Arc<dyn ChainSource>,
        SyncDriverOptions::default()
            .with_poll_interval_ms(15)
            .with_recovery_policy(
                fast_recovery_policy()
                    .with_fault_backoff_initial_ms(1)
                    .with_fault_backoff_cap_ms(2),
            ),
    )?;
    chain.report_network(Network::Mainnet);
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    let parked = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, SyncDriverPhase::Parked { .. })
    })
    .await?;
    let mut last_published_at_ms = parked.published_at_ms;
    for _ in 0..3 {
        let next = wait_for_snapshot(&mut snapshots, |snapshot| {
            matches!(snapshot.phase, SyncDriverPhase::Parked { .. })
                && snapshot.published_at_ms > last_published_at_ms
        })
        .await?;
        last_published_at_ms = next.published_at_ms;
    }

    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn close_returns_promptly_during_fault_backoff() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    chain_handle.advance_tip(BlockHeight::from(10));
    chain_handle.fail_chain_tip_next(10, || ChainSourceError::MalformedCompactBlock {
        block_height: BlockHeight::from(5),
        reason: "synthetic fault for backoff".into(),
    });

    let chain_source: Arc<dyn ChainSource> = chain;
    let driver = SyncDriver::new(
        wallet,
        chain_source,
        SyncDriverOptions::default()
            .with_poll_interval_ms(60_000)
            .with_recovery_policy(
                SyncRecoveryPolicy::default()
                    .with_fault_backoff_initial_ms(60_000)
                    .with_fault_backoff_cap_ms(60_000),
            ),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, SyncDriverPhase::Recovering { .. })
    })
    .await?;
    tokio::time::timeout(Duration::from_millis(250), handle.close())
        .await
        .map_err(|_elapsed| TestError::CloseTimedOut)??;

    Ok(())
}

async fn wait_for_snapshot(
    snapshots: &mut SyncSnapshotStream,
    mut predicate: impl FnMut(&SyncSnapshot) -> bool,
) -> Result<SyncSnapshot, TestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(snapshot) = snapshots.next().await {
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
        }
        Err(TestError::SnapshotStreamClosed)
    })
    .await
    .map_err(|_| TestError::SnapshotTimeout)?
}

/// `ChainSource` whose reported network can be shifted after construction.
///
/// The driver's construction-time network check passes, then every sync fails with
/// `WalletError::NetworkMismatch`: a parking dead end.
struct ShiftingNetworkChain {
    inner: MockChainSource,
    reported_network: Mutex<Network>,
}

impl ShiftingNetworkChain {
    fn new(network: Network) -> Self {
        Self {
            inner: MockChainSource::new(network),
            reported_network: Mutex::new(network),
        }
    }

    fn report_network(&self, network: Network) {
        *self.reported_network.lock() = network;
    }
}

#[async_trait]
impl ChainSource for ShiftingNetworkChain {
    fn network(&self) -> Network {
        *self.reported_network.lock()
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
        from_cursor: Option<ChainEventCursor>,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
        self.inner.chain_event_envelopes(from_cursor).await
    }
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("sync snapshot stream closed")]
    SnapshotStreamClosed,
    #[error("timed out waiting for sync snapshot")]
    SnapshotTimeout,
    #[error("timed out waiting for wallet event")]
    EventTimeout,
    #[error("snapshot carried an unexpected phase")]
    UnexpectedPhase,
    #[error("timed out waiting for sync driver close")]
    CloseTimedOut,
}
