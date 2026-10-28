//! A chain source whose visible tip stops moving halts the sync driver and the wallet's new
//! spends, and both resume once the tip moves, through a chain event or a fresh epoch read.
//!
//! Wallet events report entering and leaving the halt.

use std::time::Duration;
use zally_core::{BlockHeight, IdempotencyKey, Zatoshis};
use zally_testkit::MockSubmitter;

use zally_wallet::{
    ShieldTransparentPlan, SourceHalt, SyncDriverPhase, SyncSnapshot, WalletError, WalletEvent,
    WalletEventStream,
};

use super::reorg_fixtures::{
    ChainWallet, FixtureError, chain_wallet, fast_driver_options, start_driver, wait_for_driver,
};

const STALL_LIMIT_MS: u64 = 2_000;
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

async fn try_shielding(base: &ChainWallet) -> Result<WalletError, FixtureError> {
    let submitter = MockSubmitter::accepting(base.network);
    let outcome = base
        .wallet
        .shield_transparent_funds(ShieldTransparentPlan::new(
            base.account_id,
            IdempotencyKey::try_from("shield-during-halt".to_owned())
                .map_err(|_| FixtureError::Fixture("idempotency key"))?,
            Zatoshis::try_from(1_000_u64).map_err(|_| FixtureError::Fixture("zatoshis"))?,
            &submitter,
        ))
        .await;
    outcome
        .err()
        .ok_or(FixtureError::Fixture("an unfunded wallet cannot shield"))
}

fn is_halted_at(snapshot: &SyncSnapshot, height: u32) -> bool {
    matches!(
        snapshot.phase,
        SyncDriverPhase::SourceHalted {
            cause: SourceHalt::VisibleTipStalled { visible_tip_height, .. },
            ..
        } if visible_tip_height == BlockHeight::from(height)
    )
}

fn is_caught_up_at(snapshot: &SyncSnapshot, height: u32) -> bool {
    snapshot.phase == SyncDriverPhase::Waiting
        && snapshot.scanned_height == Some(BlockHeight::from(height))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_source_halts_spends_until_its_tip_moves() -> Result<(), FixtureError> {
    let base = chain_wallet(10).await?;
    let handle = start_driver(
        &base.wallet,
        &base.chain,
        None,
        fast_driver_options().with_visible_tip_stall_limit_ms(Some(STALL_LIMIT_MS)),
    )?;
    let mut seen = Vec::new();
    wait_for_driver(&handle, &mut seen, |snapshot| is_caught_up_at(snapshot, 10)).await?;
    assert!(
        !matches!(
            try_shielding(&base).await?,
            WalletError::ChainSourceHalted { .. }
        ),
        "a following source must not refuse spends"
    );

    let halted = wait_for_driver(&handle, &mut seen, |snapshot| is_halted_at(snapshot, 10)).await?;
    assert!(
        halted
            .last_fault
            .is_some_and(|fault| fault.reason.contains("stalled at height 10")),
        "the snapshot must carry the halt cause"
    );
    assert!(matches!(
        try_shielding(&base).await?,
        WalletError::ChainSourceHalted {
            cause: SourceHalt::VisibleTipStalled { .. }
        }
    ));

    if !base
        .chain_handle
        .set_chain_tips(BlockHeight::from(11), BlockHeight::from(11))
    {
        return Err(FixtureError::Fixture("settled tip above visible tip"));
    }
    wait_for_driver(&handle, &mut seen, |snapshot| is_caught_up_at(snapshot, 11)).await?;
    assert!(
        !matches!(
            try_shielding(&base).await?,
            WalletError::ChainSourceHalted { .. }
        ),
        "a tip that moved without an event must resume spends"
    );

    wait_for_driver(&handle, &mut seen, |snapshot| is_halted_at(snapshot, 11)).await?;
    base.chain_handle.advance_tip(BlockHeight::from(12));
    wait_for_driver(&handle, &mut seen, |snapshot| is_caught_up_at(snapshot, 12)).await?;
    assert!(
        !matches!(
            try_shielding(&base).await?,
            WalletError::ChainSourceHalted { .. }
        ),
        "a chain event must resume spends"
    );
    handle.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_a_halted_driver_lifts_the_spend_refusal() -> Result<(), FixtureError> {
    let base = chain_wallet(10).await?;
    let handle = start_driver(
        &base.wallet,
        &base.chain,
        None,
        fast_driver_options().with_visible_tip_stall_limit_ms(Some(STALL_LIMIT_MS)),
    )?;
    let mut seen = Vec::new();
    wait_for_driver(&handle, &mut seen, |snapshot| is_halted_at(snapshot, 10)).await?;
    handle.close().await?;
    assert!(
        !matches!(
            try_shielding(&base).await?,
            WalletError::ChainSourceHalted { .. }
        ),
        "no driver is left to lift the refusal, so closing it must"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wallet_reports_entering_and_leaving_a_source_halt() -> Result<(), FixtureError> {
    let base = chain_wallet(10).await?;
    let mut events = base.wallet.observe();
    let handle = start_driver(
        &base.wallet,
        &base.chain,
        None,
        fast_driver_options().with_visible_tip_stall_limit_ms(Some(STALL_LIMIT_MS)),
    )?;

    let halted = next_source_halt_event(&mut events).await?;
    assert!(
        matches!(
            halted,
            WalletEvent::SourceHalted {
                cause: SourceHalt::VisibleTipStalled { visible_tip_height, .. },
                ..
            } if visible_tip_height == BlockHeight::from(10)
        ),
        "entering the halt is reported with its cause: {halted:?}"
    );
    base.chain_handle.advance_tip(BlockHeight::from(11));
    assert_eq!(
        next_source_halt_event(&mut events).await?,
        WalletEvent::SourceRecovered,
        "leaving the halt is reported"
    );
    handle.close().await?;
    Ok(())
}

/// Waits for the next [`WalletEvent::SourceHalted`] or [`WalletEvent::SourceRecovered`].
async fn next_source_halt_event(
    events: &mut WalletEventStream,
) -> Result<WalletEvent, FixtureError> {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        while let Some(event) = events.next().await {
            if matches!(
                event,
                WalletEvent::SourceHalted { .. } | WalletEvent::SourceRecovered
            ) {
                return Ok(event);
            }
        }
        Err(FixtureError::Fixture("the wallet event stream closed"))
    })
    .await
    .map_err(|_elapsed| FixtureError::Fixture("timed out waiting for a source halt event"))?
}
