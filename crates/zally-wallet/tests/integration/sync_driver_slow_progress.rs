//! Faults on iterations that advanced the wallet's scanned height reset the repair ladder
//! only for environment faults; state faults keep their repair regardless of progress.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::Subscriber;
use tracing_subscriber::layer::SubscriberExt;
use zally_chain::{ChainSource, ChainSourceError};
use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zally_wallet::{
    RetryPolicy, SyncDriver, SyncDriverOptions, SyncDriverPhase, SyncRecoveryPolicy, SyncRepair,
    WalletError, WalletEvent,
};

use super::fixtures::{
    SnapshotWaitError, TestWalletError, TestWalletFixture, create_test_wallet, wait_for_snapshot,
};

fn fast_recovery_policy() -> SyncRecoveryPolicy {
    SyncRecoveryPolicy::default()
        .with_fault_backoff_initial_ms(20)
        .with_fault_backoff_cap_ms(40)
}

#[tokio::test]
async fn fault_with_scan_progress_resets_the_ladder() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::none());

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CaptureLayer {
        events: Arc::clone(&events),
    });
    let _default_guard = tracing::subscriber::set_default(subscriber);

    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    chain_handle.serve_compact_blocks();
    chain_handle.advance_tip(BlockHeight::from(50));
    chain_handle.fail_transparent_utxos_next(1, || ChainSourceError::Unavailable {
        reason: "synthetic stall after the scan committed".into(),
    });

    let driver = SyncDriver::new(
        wallet,
        chain as Arc<dyn ChainSource>,
        SyncDriverOptions::default()
            .with_poll_interval_ms(25)
            .with_recovery_policy(fast_recovery_policy()),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    let healthy = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
            && snapshot.last_fault.is_none()
            && snapshot.scanned_height == Some(BlockHeight::from(50))
    })
    .await?;
    assert_eq!(healthy.last_fault, None);
    assert_eq!(chain_handle.failures_consumed(), 1);

    let captured = events.lock().map_err(|_| TestError::Mutex)?.clone();
    assert!(
        captured
            .iter()
            .any(|e| e.contains("wallet_sync_slow_progress")),
        "the faulted iteration that advanced the scan must publish slow progress"
    );
    assert!(
        !captured.iter().any(|e| e.contains("wallet_sync_fault")),
        "a fault with scan progress must not strike the ladder"
    );
    assert!(
        !captured
            .iter()
            .any(|e| e.contains("wallet_sync_repair_started")),
        "no repair may run for a fault with scan progress"
    );

    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn state_fault_with_scan_progress_still_engages_the_ladder() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    chain_handle.serve_compact_blocks();
    chain_handle.advance_tip(BlockHeight::from(50));
    while wallet.sync(chain.as_ref()).await?.block_count > 0 {}

    let mut wallet_events = wallet.observe();
    chain_handle.advance_tip(BlockHeight::from(60));
    chain_handle.fail_transparent_utxos_next(1, || ChainSourceError::BlockHeightBelowFloor {
        requested_height: BlockHeight::from(1),
        earliest_height: BlockHeight::from(2),
    });

    let driver = SyncDriver::new(
        wallet,
        chain as Arc<dyn ChainSource>,
        SyncDriverOptions::default()
            .with_poll_interval_ms(25)
            .with_recovery_policy(fast_recovery_policy()),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(
            snapshot.phase,
            SyncDriverPhase::Recovering {
                repair: SyncRepair::Rewind,
                ..
            }
        )
    })
    .await?;

    let reorg_observed = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = wallet_events.next().await {
            if matches!(event, WalletEvent::ReorgDetected { .. }) {
                return true;
            }
        }
        false
    })
    .await
    .map_err(|_| TestError::EventTimeout)?;
    assert!(
        reorg_observed,
        "the rewind rung must run even though the faulted iteration advanced the scan"
    );

    let healthy = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
            && snapshot.last_fault.is_none()
            && snapshot.scanned_height == Some(BlockHeight::from(60))
    })
    .await?;
    assert_eq!(healthy.last_fault, None);
    assert_eq!(chain_handle.failures_consumed(), 1);

    handle.close().await?;
    Ok(())
}

struct CaptureLayer {
    events: Arc<Mutex<Vec<String>>>,
}

impl<S: Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != "zally::sync" {
            return;
        }
        let mut buf = String::new();
        let mut visitor = StringVisitor { buf: &mut buf };
        event.record(&mut visitor);
        if let Ok(mut guard) = self.events.lock() {
            guard.push(buf);
        }
    }
}

struct StringVisitor<'a> {
    buf: &'a mut String,
}

impl tracing::field::Visit for StringVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, field_value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.buf, "{}={:?} ", field.name(), field_value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, field_value: &str) {
        use std::fmt::Write;
        let _ = write!(self.buf, "{}={} ", field.name(), field_value);
    }
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("snapshot wait failed: {0}")]
    SnapshotWait(#[from] SnapshotWaitError),
    #[error("timed out waiting for wallet event")]
    EventTimeout,
    #[error("events mutex poisoned")]
    Mutex,
}
