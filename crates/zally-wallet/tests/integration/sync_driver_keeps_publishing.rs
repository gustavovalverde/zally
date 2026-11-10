//! A running sync driver keeps publishing snapshots, so an observer can tell a driver idling
//! at the tip from one whose task stopped.
//!
//! A caught-up driver republishes on every polling wakeup while the chain is silent, and a
//! chain-event subscription that never answers faults instead of wedging the driver.

use std::time::Duration;

use zally_core::BlockHeight;
use zally_wallet::{SyncDriverPhase, SyncRepair, SyncSnapshot};

use super::reorg_fixtures::{
    FixtureError, chain_wallet, fast_driver_options, start_driver, wait_for_driver,
};

const TIP_HEIGHT: u32 = 10;
/// Republished snapshots a silent chain must still produce after the driver caught up.
const REPUBLISHED_COUNT: usize = 5;
/// Longer than [`REPUBLISHED_COUNT`] polling wakeups at the fast driver's 20 ms interval, and
/// far shorter than the fixture's one-minute ceiling.
const REPUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

fn is_caught_up(snapshot: &SyncSnapshot) -> bool {
    snapshot.phase == SyncDriverPhase::Waiting
        && snapshot.scanned_height == Some(BlockHeight::from(TIP_HEIGHT))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caught_up_driver_republishes_while_the_chain_is_silent() -> Result<(), FixtureError> {
    let base = chain_wallet(TIP_HEIGHT).await?;
    let handle = start_driver(&base.wallet, &base.chain, None, fast_driver_options())?;
    let mut seen = Vec::new();
    let caught_up = wait_for_driver(&handle, &mut seen, is_caught_up).await?;

    let mut snapshots = handle.observe_status();
    let mut republished = Vec::new();
    tokio::time::timeout(REPUBLISH_TIMEOUT, async {
        while republished.len() < REPUBLISHED_COUNT {
            let Some(snapshot) = snapshots.next().await else {
                return;
            };
            if snapshot.published_at_ms > caught_up.published_at_ms {
                republished.push(snapshot);
            }
        }
    })
    .await
    .map_err(|_elapsed| FixtureError::Fixture("an idle driver stopped publishing"))?;

    assert!(
        republished.iter().all(is_caught_up),
        "a silent chain leaves the driver caught up and waiting: {republished:?}"
    );
    assert!(
        republished
            .windows(2)
            .all(|pair| pair[0].published_at_ms <= pair[1].published_at_ms),
        "each republish carries a fresh publication time: {republished:?}"
    );
    handle.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hung_chain_event_subscription_faults_instead_of_silencing_the_driver()
-> Result<(), FixtureError> {
    let base = chain_wallet(TIP_HEIGHT).await?;
    base.chain_handle.hang_chain_event_subscriptions_next(1);
    let handle = start_driver(
        &base.wallet,
        &base.chain,
        None,
        fast_driver_options().with_sync_timeout_seconds(1),
    )?;

    let mut seen = Vec::new();
    let faulted = wait_for_driver(&handle, &mut seen, |snapshot| {
        matches!(
            snapshot.phase,
            SyncDriverPhase::Recovering {
                repair: SyncRepair::Retry,
                ..
            }
        ) && snapshot.last_fault.is_some()
    })
    .await?;
    let reason = faulted
        .last_fault
        .map(|fault| fault.reason)
        .unwrap_or_default();
    assert!(
        reason.contains("chain-event subscription did not answer within 1 seconds"),
        "the hung subscription must surface as a fault naming it, got {reason:?}"
    );

    wait_for_driver(&handle, &mut seen, is_caught_up).await?;
    tokio::time::timeout(REPUBLISH_TIMEOUT, async {
        while base.chain_handle.chain_event_subscription_starts().len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_elapsed| FixtureError::Fixture("the driver never reopened the stream"))?;
    handle.close().await?;
    Ok(())
}
