//! An announced reorg rewinds the wallet to its fork point at any depth.
//!
//! The rewind keeps the payment ledger, reports what left the chain, and resubmits the
//! withdrawal it reverted. A retried withdrawal is refused by its idempotency key until the
//! withdrawal can never be mined, judged at the settled and fully scanned height; only then
//! does the key pay again.
//!
//! Each case shields a transparent UTXO through the wallet so the chain holds a withdrawal
//! the wallet sent and a deposit it received, mined two blocks below the tip, then reorgs the
//! chain `depth` blocks deep. Depth 900 sits inside the 1,000-block window Zinder keeps for a
//! Zebra 7 node; the mock serves any depth.

use zally_core::{BlockHeight, HoldId, IdempotencyKey, TxId, Zatoshis};
use zally_storage::WalletStorage;
use zally_wallet::{
    ShieldTransparentPlan, SyncDriverPhase, SyncHandle, SyncRepair, SyncSnapshot, WalletEvent,
};

use super::reorg_fixtures::{
    EventLog, FixtureError, FundedChain, fast_driver_options, funded_chain, record_hold,
    start_driver, wait_for_driver,
};

/// Empty blocks below the shielding transaction, enough for a 900-block reorg to reach it.
const PRE_FUNDING_TIP: u32 = 1_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reorg_five_blocks_deep_rewinds_to_the_fork_point() -> Result<(), FixtureError> {
    announced_reorg_keeps_the_ledger(5).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reorg_150_blocks_deep_rewinds_to_the_fork_point() -> Result<(), FixtureError> {
    announced_reorg_keeps_the_ledger(150).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reorg_900_blocks_deep_rewinds_to_the_fork_point() -> Result<(), FixtureError> {
    announced_reorg_keeps_the_ledger(900).await
}

async fn announced_reorg_keeps_the_ledger(depth: u32) -> Result<(), FixtureError> {
    let funded = funded_chain(PRE_FUNDING_TIP).await?;
    let base = &funded.base;
    record_hold(&base.storage, base.account_id).await?;
    let events = EventLog::record(base.wallet.observe());
    let handle = start_driver(
        &base.wallet,
        &base.chain,
        Some(&funded.submitter),
        fast_driver_options(),
    )?;
    let mut seen = Vec::new();
    wait_until_scanned_to(&handle, &mut seen, funded.tip_height).await?;
    let deposit_value = deposit_value(&funded).await?;
    assert!(confirmed_at(&events, funded.shielding_tx_id).is_some());

    let reorg_from = funded.tip_height - depth + 1;
    let new_tip = funded.tip_height + 1;
    base.chain_handle
        .trigger_reorg(BlockHeight::from(reorg_from), BlockHeight::from(new_tip));
    seen.clear();
    wait_until_scanned_to(&handle, &mut seen, new_tip).await?;
    events
        .wait_for(|event| {
            matches!(event, WalletEvent::TransactionReverted { tx_id, .. }
                if *tx_id == funded.shielding_tx_id)
        })
        .await?;

    assert_rewound_once_to(&events, &seen, BlockHeight::from(reorg_from - 1));
    assert_reverted_activity_reported(&funded, &events, deposit_value).await?;
    assert_ledger_kept(&funded).await?;
    assert_eq!(
        funded.submitter.handle().submitted_bytes(),
        vec![raw_shielding(&funded)?; 2],
        "the reverted withdrawal must be submitted again, once"
    );
    assert_eq!(
        retry_withdrawal(&funded).await?,
        funded.shielding_tx_id,
        "a withdrawal that can still be mined must not be paid twice"
    );
    let raw_withdrawal = raw_shielding(&funded)?;
    assert!(
        funded
            .submitter
            .handle()
            .submitted_bytes()
            .iter()
            .all(|raw_tx| *raw_tx == raw_withdrawal),
        "a retry submits the same withdrawal again and builds nothing new"
    );

    let remined_height = new_tip + 1;
    funded.mine_shielding_at(remined_height)?;
    base.chain_handle
        .advance_tip(BlockHeight::from(remined_height + 1));
    wait_until_scanned_to(&handle, &mut seen, remined_height + 1).await?;
    assert_remined_at(&funded, &events, remined_height).await?;
    handle.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reverted_withdrawal_releases_its_key_once_it_can_never_be_mined()
-> Result<(), FixtureError> {
    let funded = funded_chain(200).await?;
    let base = &funded.base;
    let hold_id = record_hold(&base.storage, base.account_id).await?;
    base.storage
        .finalize_hold(hold_id, funded.shielding_tx_id)
        .await?;
    let events = EventLog::record(base.wallet.observe());
    let handle = start_driver(&base.wallet, &base.chain, None, fast_driver_options())?;
    let mut seen = Vec::new();
    wait_until_scanned_to(&handle, &mut seen, funded.tip_height).await?;

    let expiry_height = u32::from(funded.shielding.expiry_height());
    base.chain_handle.trigger_reorg(
        BlockHeight::from(funded.funding_height),
        BlockHeight::from(funded.tip_height + 1),
    );
    let past_expiry = expiry_height + 1;
    if !base.chain_handle.commit_tips(
        BlockHeight::from(past_expiry),
        BlockHeight::from(expiry_height - 1),
    ) {
        return Err(FixtureError::Fixture("settled tip above visible tip"));
    }
    wait_until_scanned_to(&handle, &mut seen, past_expiry).await?;
    assert!(
        !events
            .snapshot()
            .iter()
            .any(|event| matches!(event, WalletEvent::PendingBroadcastUnmineable { .. })),
        "a withdrawal is not dead while a reorg could still mine it below its expiry"
    );
    assert_eq!(retry_withdrawal(&funded).await?, funded.shielding_tx_id);
    assert_eq!(funded.submitter.handle().submission_count(), 1);

    base.chain_handle
        .advance_tip(BlockHeight::from(past_expiry + 1));
    events
        .wait_for(|event| {
            matches!(event, WalletEvent::PendingBroadcastUnmineable { tx_id, reason }
                if *tx_id == funded.shielding_tx_id
                    && reason.contains(&format!("expired at height {expiry_height}")))
        })
        .await?;
    assert_settled_unmineable_once(&funded, &events, hold_id).await?;
    let replacement = retry_withdrawal(&funded).await?;
    assert_ne!(
        replacement, funded.shielding_tx_id,
        "the released key must create a replacement withdrawal"
    );
    assert_eq!(funded.submitter.handle().submission_count(), 2);
    assert_eq!(
        retry_withdrawal(&funded).await?,
        replacement,
        "the key now refuses to pay the replacement twice"
    );
    handle.close().await?;
    Ok(())
}

/// The verdict reports the dead withdrawal once, releases its key, and reopens the hold that
/// was finalized with it, since that dispense did not happen.
async fn assert_settled_unmineable_once(
    funded: &FundedChain,
    events: &EventLog,
    hold_id: HoldId,
) -> Result<(), FixtureError> {
    let storage = &funded.base.storage;
    events
        .wait_for(|event| {
            matches!(event, WalletEvent::HoldDispenseUnmineable {
                hold_id: reopened, tx_id, is_reopened: true, ..
            } if *reopened == hold_id && *tx_id == funded.shielding_tx_id)
        })
        .await?;
    let unmineable_reports = events
        .snapshot()
        .iter()
        .filter(|event| {
            matches!(event, WalletEvent::PendingBroadcastUnmineable { tx_id, .. }
                if *tx_id == funded.shielding_tx_id)
        })
        .count();
    assert_eq!(
        unmineable_reports, 1,
        "a dead withdrawal is reported exactly once"
    );
    assert!(storage.list_reverted_broadcasts().await?.is_empty());
    assert_eq!(
        storage
            .find_idempotent_submission(&funded.shielding_key)
            .await?,
        None,
        "a settled expiry releases the withdrawal's idempotency key"
    );
    assert!(
        storage
            .list_active_holds(funded.base.account_id)
            .await?
            .iter()
            .any(|hold| hold.hold_id == hold_id),
        "the hold of a dispense that never happened must be active again"
    );
    Ok(())
}

async fn wait_until_scanned_to(
    handle: &SyncHandle,
    seen: &mut Vec<SyncSnapshot>,
    height: u32,
) -> Result<SyncSnapshot, FixtureError> {
    wait_for_driver(handle, seen, |snapshot| {
        snapshot.phase == SyncDriverPhase::Waiting
            && snapshot.scanned_height == Some(BlockHeight::from(height))
    })
    .await
}

/// The only rewind is to the announced fork point.
///
/// A scan can meet the replacement chain before the reorg event arrives; the event then
/// replaces the ladder's relative rewind, so the ladder may record a fault but never rewinds,
/// rebuilds, or parks.
fn assert_rewound_once_to(events: &EventLog, seen: &[SyncSnapshot], fork_height: BlockHeight) {
    let rewinds: Vec<BlockHeight> = events
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
        .collect();
    assert_eq!(
        rewinds,
        vec![fork_height],
        "the wallet must rewind once, to exactly the announced fork point"
    );
    assert!(
        !seen.iter().any(|snapshot| matches!(
            snapshot.phase,
            SyncDriverPhase::Recovering {
                repair: SyncRepair::RescanFromBirthday,
                ..
            } | SyncDriverPhase::Parked { .. }
        )),
        "an announced reorg must never escalate to a rebuild or park"
    );
}

async fn assert_reverted_activity_reported(
    funded: &FundedChain,
    events: &EventLog,
    deposit_value: Zatoshis,
) -> Result<(), FixtureError> {
    let base = &funded.base;
    assert!(events.snapshot().iter().any(|event| matches!(
        event,
        WalletEvent::TransactionReverted { tx_id, mined_height }
            if *tx_id == funded.shielding_tx_id
                && *mined_height == BlockHeight::from(funded.funding_height)
    )));
    assert!(
        events.snapshot().iter().any(|event| matches!(
            event,
            WalletEvent::ShieldedReceiveReverted { tx_id, value_zat, .. }
                if *tx_id == funded.shielding_tx_id && *value_zat == deposit_value
        )),
        "the reverted deposit must be reported to the host"
    );
    assert!(
        base.wallet
            .list_shielded_receives(base.account_id)
            .await?
            .is_empty(),
        "the deposit must leave the receive history with its block"
    );
    assert_eq!(
        base.wallet
            .get_account_balance(base.account_id)
            .await?
            .sapling_zat,
        Zatoshis::zero(),
        "the balance must stop counting the reverted deposit"
    );
    Ok(())
}

async fn assert_ledger_kept(funded: &FundedChain) -> Result<(), FixtureError> {
    let storage = &funded.base.storage;
    assert_eq!(
        storage
            .find_idempotent_submission(&funded.shielding_key)
            .await?,
        Some(funded.shielding_tx_id),
        "the withdrawal's idempotency record must survive the rewind"
    );
    let hold_request = IdempotencyKey::try_from("hold-request-0001".to_owned())
        .map_err(|_| FixtureError::Fixture("hold request id"))?;
    assert!(
        storage
            .find_hold_by_request_id(&hold_request)
            .await?
            .is_some(),
        "holds must survive the rewind"
    );
    Ok(())
}

async fn assert_remined_at(
    funded: &FundedChain,
    events: &EventLog,
    remined_height: u32,
) -> Result<(), FixtureError> {
    let base = &funded.base;
    assert_eq!(
        confirmed_at(events, funded.shielding_tx_id),
        Some(BlockHeight::from(remined_height)),
        "the replacement chain must confirm the withdrawal again"
    );
    assert!(
        base.storage.list_reverted_broadcasts().await?.is_empty(),
        "a withdrawal mined again leaves the reverted-broadcast ledger"
    );
    assert_eq!(
        base.wallet
            .list_shielded_receives(base.account_id)
            .await?
            .len(),
        1,
        "the deposit counts again once the replacement chain mines it"
    );
    Ok(())
}

/// Retries the withdrawal with its original idempotency key and returns the transaction id
/// the wallet reports.
async fn retry_withdrawal(funded: &FundedChain) -> Result<TxId, FixtureError> {
    let outcome = funded
        .base
        .wallet
        .shield_transparent_funds(
            ShieldTransparentPlan::new(
                funded.base.account_id,
                funded.shielding_key.clone(),
                Zatoshis::try_from(1_000_u64).map_err(|_| FixtureError::Fixture("zatoshis"))?,
                funded.submitter.as_ref(),
            )
            .with_destination_pool(zally_chain::ShieldedPool::Sapling),
        )
        .await?;
    Ok(outcome.tx_id())
}

fn confirmed_at(events: &EventLog, transaction: TxId) -> Option<BlockHeight> {
    events.snapshot().into_iter().rev().find_map(|event| {
        if let WalletEvent::TransactionConfirmed {
            tx_id,
            confirmed_at_height,
        } = event
            && tx_id == transaction
        {
            Some(confirmed_at_height)
        } else {
            None
        }
    })
}

fn raw_shielding(funded: &FundedChain) -> Result<Vec<u8>, FixtureError> {
    let mut raw = Vec::new();
    funded.shielding.write(&mut raw)?;
    Ok(raw)
}

async fn deposit_value(funded: &FundedChain) -> Result<Zatoshis, FixtureError> {
    let receives = funded
        .base
        .wallet
        .list_shielded_receives(funded.base.account_id)
        .await?;
    receives
        .iter()
        .find(|receive| receive.tx_id == funded.shielding_tx_id)
        .map(|receive| receive.value)
        .ok_or(FixtureError::Fixture("the deposit was not received"))
}
