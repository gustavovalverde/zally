//! A chain source with a shallower reorg window than the node's deepest reorg refuses spends.
//!
//! A released broadcast could still be mined after its replacement was paid. The sync driver
//! reads the window when it starts and each time it reopens the chain-event stream, and spends
//! resume once the window is raised.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use futures_util::{StreamExt as _, stream};
use tokio::sync::Notify;
use zally_chain::{
    BlockHeightRange, ChainEpoch, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainEventStreamStart, ChainSource, ChainSourceError, CompactBlockStream, ShieldedPool,
    SubtreeIndex, SubtreeRoot, TransactionStatus, TransparentUtxo,
};
use zally_core::{BlockHeight, IdempotencyKey, Network, TreeStateArtifact, TxId, Zatoshis};
use zally_storage::WalletStorage as _;
use zally_testkit::{MockChainSource, MockSubmitter};
use zally_wallet::{
    ShieldTransparentPlan, SyncDriver, SyncDriverOptions, SyncDriverPhase, SyncHandle,
    SyncSnapshot, WalletError, WalletEvent, WalletEventStream,
};

use super::reorg_fixtures::{
    ChainWallet, EventLog, FixtureError, chain_wallet, fast_driver_options, funded_chain,
    wait_for_driver,
};

const SHALLOW_WINDOW_BLOCKS: u32 = 100;
const REQUIRED_WINDOW_BLOCKS: u32 = 1_000;
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shallow_source_reorg_window_refuses_spends_until_it_is_raised()
-> Result<(), FixtureError> {
    let base = chain_wallet(10).await?;
    base.chain_handle
        .set_reorg_window_blocks(SHALLOW_WINDOW_BLOCKS);
    let stream_fault = Arc::new(Notify::new());
    let mut events = base.wallet.observe();
    let handle = start_driver(&base, Arc::clone(&stream_fault), fast_driver_options())?;

    let too_shallow = WalletEvent::SourceReorgWindowTooShallow {
        reorg_window_blocks: SHALLOW_WINDOW_BLOCKS,
        required_blocks: REQUIRED_WINDOW_BLOCKS,
    };
    wait_for_event(&mut events, &too_shallow).await?;
    let refused = try_shielding(&base).await?;
    assert!(
        matches!(
            refused,
            WalletError::SourceReorgWindowTooShallow {
                reorg_window_blocks: SHALLOW_WINDOW_BLOCKS,
                required_blocks: REQUIRED_WINDOW_BLOCKS,
            }
        ),
        "a shallow window refuses spends: {refused:?}"
    );
    assert!(
        !refused.is_retryable(),
        "the operator must act: {refused:?}"
    );

    base.chain_handle
        .set_reorg_window_blocks(REQUIRED_WINDOW_BLOCKS);
    stream_fault.notify_one();
    wait_for_event(
        &mut events,
        &WalletEvent::SourceReorgWindowSufficient {
            reorg_window_blocks: REQUIRED_WINDOW_BLOCKS,
            required_blocks: REQUIRED_WINDOW_BLOCKS,
        },
    )
    .await?;
    let resumed = try_shielding(&base).await?;
    assert!(
        !matches!(resumed, WalletError::SourceReorgWindowTooShallow { .. }),
        "the reopened stream reads the raised window and spends resume: {resumed:?}"
    );
    handle.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disabled_check_accepts_a_shallow_source_reorg_window() -> Result<(), FixtureError> {
    let base = chain_wallet(10).await?;
    base.chain_handle
        .set_reorg_window_blocks(SHALLOW_WINDOW_BLOCKS);
    let mut events = base.wallet.observe();
    let handle = start_driver(
        &base,
        Arc::new(Notify::new()),
        fast_driver_options().with_minimum_source_reorg_window_blocks(None),
    )?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.next()).await {
        assert!(
            !matches!(event, WalletEvent::SourceReorgWindowTooShallow { .. }),
            "a disabled check reports nothing"
        );
    }
    let outcome = try_shielding(&base).await?;
    assert!(
        !matches!(outcome, WalletError::SourceReorgWindowTooShallow { .. }),
        "a disabled check refuses nothing: {outcome:?}"
    );
    handle.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shallow_source_reorg_window_holds_unmineable_verdicts() -> Result<(), FixtureError> {
    let funded = funded_chain(200).await?;
    let base = &funded.base;
    base.chain_handle
        .set_reorg_window_blocks(SHALLOW_WINDOW_BLOCKS);
    let stream_fault = Arc::new(Notify::new());
    let events = EventLog::record(base.wallet.observe());
    let handle = start_driver(base, Arc::clone(&stream_fault), fast_driver_options())?;
    let mut seen = Vec::new();
    wait_for_driver(&handle, &mut seen, |snapshot| {
        is_scanned_to(snapshot, funded.tip_height)
    })
    .await?;

    // A reorg removes the withdrawal, and the settled tip moves past its expiry.
    let expiry_height = u32::from(funded.shielding.expiry_height());
    base.chain_handle.trigger_reorg(
        BlockHeight::from(funded.funding_height),
        BlockHeight::from(funded.tip_height + 1),
    );
    let past_expiry = expiry_height + 1;
    if !base.chain_handle.commit_tips(
        BlockHeight::from(past_expiry),
        BlockHeight::from(past_expiry),
    ) {
        return Err(FixtureError::Fixture("settled tip above visible tip"));
    }
    wait_for_driver(&handle, &mut seen, |snapshot| {
        is_scanned_to(snapshot, past_expiry)
    })
    .await?;
    assert!(
        !events
            .snapshot()
            .iter()
            .any(|event| matches!(event, WalletEvent::PendingBroadcastUnmineable { .. })),
        "a source that can still replace its settled tip cannot back an unmineable verdict"
    );
    assert_eq!(
        base.storage
            .find_idempotent_submission(&funded.shielding_key)
            .await?,
        Some(funded.shielding_tx_id),
        "the withdrawal keeps its key while the verdict is held"
    );

    base.chain_handle
        .set_reorg_window_blocks(REQUIRED_WINDOW_BLOCKS);
    stream_fault.notify_one();
    events
        .wait_for(|event| {
            matches!(event, WalletEvent::PendingBroadcastUnmineable { tx_id, .. }
                if *tx_id == funded.shielding_tx_id)
        })
        .await?;
    handle.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unread_source_reorg_window_refuses_spends() -> Result<(), FixtureError> {
    let base = chain_wallet(10).await?;
    let handle =
        start_driver_reading_window(&base, Arc::new(Notify::new()), fast_driver_options(), false)?;
    let mut seen = Vec::new();
    wait_for_driver(&handle, &mut seen, |snapshot| is_scanned_to(snapshot, 10)).await?;
    let refused = try_shielding(&base).await?;
    assert!(
        matches!(
            refused,
            WalletError::SourceReorgWindowUnverified {
                required_blocks: REQUIRED_WINDOW_BLOCKS
            }
        ),
        "a window the driver could not read refuses spends: {refused:?}"
    );
    handle.close().await?;
    assert!(
        !matches!(
            try_shielding(&base).await?,
            WalletError::SourceReorgWindowUnverified { .. }
        ),
        "closing the driver lifts the refusal"
    );
    Ok(())
}

fn start_driver(
    base: &ChainWallet,
    stream_fault: Arc<Notify>,
    options: SyncDriverOptions,
) -> Result<SyncHandle, FixtureError> {
    start_driver_reading_window(base, stream_fault, options, true)
}

fn start_driver_reading_window(
    base: &ChainWallet,
    stream_fault: Arc<Notify>,
    options: SyncDriverOptions,
    is_window_readable: bool,
) -> Result<SyncHandle, FixtureError> {
    let chain: Arc<dyn ChainSource> = Arc::new(FaultingFirstStreamChain {
        inner: Arc::clone(&base.chain),
        stream_fault,
        stream_open_count: AtomicU32::new(0),
        is_window_readable: AtomicBool::new(is_window_readable),
    });
    Ok(SyncDriver::new(base.wallet.clone(), chain, options)?.sync_continuously())
}

fn is_scanned_to(snapshot: &SyncSnapshot, height: u32) -> bool {
    snapshot.phase == SyncDriverPhase::Waiting
        && snapshot.scanned_height == Some(BlockHeight::from(height))
}

/// Shields with an unfunded wallet; returns the error that refused it.
async fn try_shielding(base: &ChainWallet) -> Result<WalletError, FixtureError> {
    let submitter = MockSubmitter::accepting(base.network);
    let outcome = base
        .wallet
        .shield_transparent_funds(ShieldTransparentPlan::new(
            base.account_id,
            IdempotencyKey::try_from("shield-on-shallow-window".to_owned())
                .map_err(|_| FixtureError::Fixture("idempotency key"))?,
            Zatoshis::try_from(1_000_u64).map_err(|_| FixtureError::Fixture("zatoshis"))?,
            &submitter,
        ))
        .await;
    outcome
        .err()
        .ok_or(FixtureError::Fixture("an unfunded wallet cannot shield"))
}

async fn wait_for_event(
    events: &mut WalletEventStream,
    expected: &WalletEvent,
) -> Result<(), FixtureError> {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        while let Some(event) = events.next().await {
            if &event == expected {
                return Ok(());
            }
        }
        Err(FixtureError::Fixture("the wallet event stream closed"))
    })
    .await
    .map_err(|_elapsed| FixtureError::Fixture("timed out waiting for a wallet event"))?
}

/// Delegates to a mock chain, with a first chain-event stream that can be made to fault.
///
/// The first stream faults once `stream_fault` is notified, so the driver reopens it, and the
/// reorg window read fails while `is_window_readable` is false.
struct FaultingFirstStreamChain {
    inner: Arc<MockChainSource>,
    stream_fault: Arc<Notify>,
    stream_open_count: AtomicU32,
    is_window_readable: AtomicBool,
}

#[async_trait::async_trait]
impl ChainSource for FaultingFirstStreamChain {
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
        let is_first_stream = self.stream_open_count.fetch_add(1, Ordering::SeqCst) == 0;
        let delivered = self.inner.chain_event_envelopes(start).await?;
        if !is_first_stream {
            return Ok(delivered);
        }
        let stream_fault = Arc::clone(&self.stream_fault);
        let interrupted = async move { stream_fault.notified().await };
        Ok(Box::pin(delivered.take_until(interrupted).chain(
            stream::once(async {
                Err::<ChainEventEnvelope, _>(ChainSourceError::Unavailable {
                    reason: "chain-event stream interrupted".into(),
                })
            }),
        )))
    }

    async fn reorg_window_blocks(&self) -> Result<u32, ChainSourceError> {
        if !self.is_window_readable.load(Ordering::SeqCst) {
            return Err(ChainSourceError::Unavailable {
                reason: "server info unavailable".into(),
            });
        }
        self.inner.reorg_window_blocks().await
    }
}
