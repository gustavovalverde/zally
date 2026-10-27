//! Reorgs no chain event announced reach the repair ladder, and the ladder keeps the payment
//! ledger at every rung.
//!
//! A reorg deeper than 100 blocks converges through compounding relative rewinds, including
//! past a rewind that fails mid-ladder, without rebuilding or parking. When rewinds cannot
//! cure a fault the ladder still rebuilds from the birthday, and the rebuild keeps
//! idempotency records, holds, and the chain-event cursor.

use std::time::Duration;

use zally_chain::ChainSourceError;
use zally_core::{BlockHeight, IdempotencyKey, TxId};
use zally_storage::WalletStorage;
use zally_wallet::{SyncDriverPhase, SyncRecoveryPolicy, SyncRepair, SyncSnapshot, WalletEvent};

use super::reorg_fixtures::{
    ChainWallet, EventLog, FixtureError, chain_wallet, fast_driver_options, record_hold,
    start_driver, wait_for_driver,
};

const PAYOUT_TX_ID: [u8; 32] = [0xcd; 32];

async fn record_ledger(base: &ChainWallet) -> Result<IdempotencyKey, FixtureError> {
    let key = IdempotencyKey::try_from("payout-before-the-reorg".to_owned())
        .map_err(|_| FixtureError::Fixture("idempotency key"))?;
    base.storage
        .record_idempotent_submission(key.clone(), TxId::from_bytes(PAYOUT_TX_ID))
        .await?;
    record_hold(&base.storage, base.account_id).await?;
    Ok(key)
}

async fn assert_ledger_kept(base: &ChainWallet, key: &IdempotencyKey) -> Result<(), FixtureError> {
    assert_eq!(
        base.storage.find_idempotent_submission(key).await?,
        Some(TxId::from_bytes(PAYOUT_TX_ID)),
        "idempotency records must survive every repair rung"
    );
    assert_eq!(
        base.storage.list_active_holds(base.account_id).await?.len(),
        1,
        "holds must survive every repair rung"
    );
    Ok(())
}

fn saw_phase(seen: &[SyncSnapshot], phase: impl Fn(&SyncDriverPhase) -> bool) -> bool {
    seen.iter().any(|snapshot| phase(&snapshot.phase))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unannounced_150_block_reorg_converges_through_the_ladder() -> Result<(), FixtureError> {
    let base = chain_wallet(300).await?;
    let key = record_ledger(&base).await?;
    let events = EventLog::record(base.wallet.observe());
    let handle = start_driver(&base.wallet, &base.chain, None, fast_driver_options())?;
    let mut seen = Vec::new();
    wait_for_driver(&handle, &mut seen, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
            && snapshot.scanned_height == Some(BlockHeight::from(300))
    })
    .await?;

    base.chain_handle
        .reorg_silently(BlockHeight::from(151), BlockHeight::from(301));
    base.chain_handle
        .fail_tree_state_at_next(BlockHeight::from(280), 1, || {
            ChainSourceError::BlockHeightBelowFloor {
                requested_height: BlockHeight::from(280),
                earliest_height: BlockHeight::from(281),
            }
        });
    base.chain_handle.advance_tip(BlockHeight::from(302));
    seen.clear();
    wait_for_driver(&handle, &mut seen, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
            && snapshot.scanned_height == Some(BlockHeight::from(302))
            && snapshot.last_fault.is_none()
    })
    .await?;
    handle.close().await?;

    assert!(saw_phase(&seen, |phase| matches!(
        phase,
        SyncDriverPhase::Recovering {
            repair: SyncRepair::Rewind,
            ..
        }
    )));
    assert!(
        !saw_phase(&seen, |phase| matches!(
            phase,
            SyncDriverPhase::Recovering {
                repair: SyncRepair::RescanFromBirthday,
                ..
            } | SyncDriverPhase::Parked { .. }
        )),
        "a reorg Zebra can serve must converge without a rebuild or a park"
    );
    assert_eq!(
        base.chain_handle.failures_consumed(),
        1,
        "the failed rewind must have been met and survived"
    );
    let deepest_rewind = events
        .snapshot()
        .into_iter()
        .filter_map(|event| {
            if let WalletEvent::ReorgDetected {
                rolled_back_to_height,
                ..
            } = event
            {
                Some(rolled_back_to_height)
            } else {
                None
            }
        })
        .min();
    assert!(
        deepest_rewind.is_some_and(|height| height < BlockHeight::from(151)),
        "compounding rewinds must step below the fork: {deepest_rewind:?}"
    );
    assert_ledger_kept(&base, &key).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_birthday_rescan_keeps_the_payment_ledger() -> Result<(), FixtureError> {
    let base = chain_wallet(40).await?;
    let key = record_ledger(&base).await?;
    let events = EventLog::record(base.wallet.observe());
    base.chain_handle
        .fail_current_epoch_next(4, || ChainSourceError::BlockHeightBelowFloor {
            requested_height: BlockHeight::from(10),
            earliest_height: BlockHeight::from(20),
        });
    let options = fast_driver_options().with_recovery_policy(
        SyncRecoveryPolicy::default()
            .with_escalate_after_faults(1)
            .with_max_rescan_attempts(2)
            .with_fault_backoff_initial_ms(10)
            .with_fault_backoff_cap_ms(20)
            .with_park_reprobe_ms(None),
    );
    let handle = start_driver(&base.wallet, &base.chain, None, options)?;
    let mut seen = Vec::new();
    wait_for_driver(&handle, &mut seen, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
            && snapshot.scanned_height == Some(BlockHeight::from(40))
            && snapshot.last_fault.is_none()
    })
    .await?;
    handle.close().await?;

    events
        .wait_for(|event| matches!(event, WalletEvent::DerivedStateReset { .. }))
        .await?;
    assert!(saw_phase(&seen, |phase| matches!(
        phase,
        SyncDriverPhase::Recovering {
            repair: SyncRepair::RescanFromBirthday,
            ..
        }
    )));
    assert_ledger_kept(&base, &key).await?;
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        base.storage.find_chain_event_cursor().await?,
        Some(
            base.chain_handle
                .latest_chain_event_sequence()
                .to_be_bytes()
                .to_vec()
        ),
        "the chain-event cursor must survive the rebuild"
    );
    Ok(())
}
