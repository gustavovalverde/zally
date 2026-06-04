//! Wallet sync loop.
//!
//! `Wallet::sync` drives `zcash_client_backend::data_api::chain::scan_cached_blocks` through
//! the storage-side `WalletStorage::scan_blocks` extension. The chain source streams compact
//! blocks; the wallet drains them, builds a `ChainState`, and hands both to the storage layer,
//! which runs the upstream scanner against the live `WalletDb`.
//!
//! Each call re-scans from the wallet's last fully-scanned height up to the current chain
//! tip. The `ChainState` is rebuilt from the embedded genesis frontier on every call: correct,
//! linear-in-tip-height.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt as _, future};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, timeout};
use tokio_stream::wrappers::WatchStream;
use zally_chain::{
    BlockHeightRange, ChainEventCursor, ChainEventEnvelope, ChainEventEnvelopeStream, ChainSource,
    ChainSourceError, ChainState, FailurePosture, ShieldedPool, SubtreeIndex,
};
use zally_core::{BlockHeight, Network};
use zally_storage::{ScanRequest, TransparentUtxoRow};
use zcash_client_backend::data_api::scanning::ScanPriority;

use crate::error::WalletError;
use crate::event::WalletEvent;
use crate::retry::with_breaker_and_retry;
use crate::status::{SyncStatus, WalletStatus};
use crate::wallet::Wallet;

/// Maximum compact blocks scanned in one `Wallet::sync` call. A suggested range larger than
/// this is scanned across successive calls; the driver loops until the scan queue drains.
const MAX_BLOCKS_PER_SYNC: u32 = 1_000;

/// Subtree-root page size for the per-cycle backfill. Zinder clamps to its own page cap.
const SUBTREE_ROOT_PAGE: u32 = 128;

/// Blocks to rewind below a scan-time continuity error before re-planning.
///
/// A `PrevHashMismatch` at height `H` means the wallet's block at `H - 1` is orphaned, so the
/// rewind must drop it; a margin lets a multi-block reorg re-converge in one pass.
/// [`WalletStorage::truncate_to_chain_state`] lands the wallet at exactly the rewind height
/// (unlike a checkpoint-snapping truncation, which lands below the target and spirals), so the
/// margin is a reorg-depth heuristic, not a correctness crutch.
const REORG_REWIND_BLOCKS: u32 = 10;

struct ScanContext {
    blocks: Vec<zcash_client_backend::proto::compact_formats::CompactBlock>,
    scanned_from: BlockHeight,
    target_height: BlockHeight,
    block_count: u64,
}

/// Summary of a `Wallet::sync` run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncOutcome {
    /// Height the wallet was scanned from (exclusive of the prior scan progress).
    pub scanned_from_height: BlockHeight,
    /// Height the wallet finished scanning at.
    pub scanned_to_height: BlockHeight,
    /// Number of blocks scanned during this run.
    pub block_count: u64,
    /// Number of transparent UTXOs refreshed during this run.
    pub transparent_utxo_count: u64,
    /// Number of reorgs observed during this run.
    pub reorgs_observed: u32,
}

/// Policy for a long-lived [`SyncDriver`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncDriverOptions {
    /// Milliseconds between polling wakeups when no chain event is received.
    pub poll_interval_ms: u64,
    /// Maximum [`Wallet::sync`] calls made for one wakeup.
    pub max_sync_iterations_per_wake_count: u32,
    /// Maximum seconds one [`Wallet::sync`] call may run before the driver retries later.
    pub sync_timeout_seconds: u64,
}

impl SyncDriverOptions {
    /// Returns options with `poll_interval_ms` replaced.
    #[must_use]
    pub const fn with_poll_interval_ms(self, poll_interval_ms: u64) -> Self {
        Self {
            poll_interval_ms,
            ..self
        }
    }

    /// Returns options with `max_sync_iterations_per_wake_count` replaced.
    #[must_use]
    pub const fn with_max_sync_iterations_per_wake_count(
        self,
        max_sync_iterations_per_wake_count: u32,
    ) -> Self {
        Self {
            max_sync_iterations_per_wake_count,
            ..self
        }
    }

    /// Returns options with `sync_timeout_seconds` replaced.
    #[must_use]
    pub const fn with_sync_timeout_seconds(self, sync_timeout_seconds: u64) -> Self {
        Self {
            sync_timeout_seconds,
            ..self
        }
    }

    fn normalized(self) -> Self {
        Self {
            poll_interval_ms: self.poll_interval_ms.max(1),
            max_sync_iterations_per_wake_count: self.max_sync_iterations_per_wake_count.max(1),
            sync_timeout_seconds: self.sync_timeout_seconds.max(1),
        }
    }
}

impl Default for SyncDriverOptions {
    fn default() -> Self {
        Self {
            poll_interval_ms: 5_000,
            max_sync_iterations_per_wake_count: 1_000,
            sync_timeout_seconds: 120,
        }
    }
}

/// Lifecycle state of a running [`SyncDriver`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SyncDriverStatus {
    /// The driver task has been created and is opening its chain-event stream.
    Starting,
    /// The driver is running one or more [`Wallet::sync`] iterations.
    Syncing,
    /// The driver is waiting for a chain event or the next polling wakeup.
    Waiting,
    /// The driver is closing after the caller requested shutdown.
    Closing,
    /// The driver closed cleanly.
    Closed,
    /// The driver stopped after a terminal error.
    Failed {
        /// Operator-facing posture for the terminal failure.
        posture: FailurePosture,
    },
}

/// Cloneable error summary carried by [`SyncSnapshot`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncErrorSnapshot {
    /// Error description safe for logs and status pages.
    pub reason: String,
    /// Operator-facing posture for this failure.
    pub posture: FailurePosture,
}

/// Current observable state of a [`SyncDriver`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncSnapshot {
    /// Network this driver is bound to.
    pub network: Network,
    /// Driver lifecycle status.
    pub driver_status: SyncDriverStatus,
    /// Wallet scan status derived from persisted progress.
    pub sync_status: SyncStatus,
    /// Highest block height the wallet has scanned, if any.
    pub scanned_height: Option<BlockHeight>,
    /// Chain tip the wallet most recently observed, if any.
    pub safe_chain_tip_height: Option<BlockHeight>,
    /// Number of blocks between `scanned_height` and `safe_chain_tip_height`, if known.
    pub lag_blocks: Option<u32>,
    /// Most recent [`Wallet::sync`] run summary.
    pub last_sync_outcome: Option<SyncOutcome>,
    /// Most recent retryable error, or the terminal error if the driver failed.
    pub last_error: Option<SyncErrorSnapshot>,
}

impl SyncSnapshot {
    fn starting(network: Network) -> Self {
        Self {
            network,
            driver_status: SyncDriverStatus::Starting,
            sync_status: SyncStatus::NotStarted,
            scanned_height: None,
            safe_chain_tip_height: None,
            lag_blocks: None,
            last_sync_outcome: None,
            last_error: None,
        }
    }

    fn from_wallet_status(
        driver_status: SyncDriverStatus,
        wallet_status: &WalletStatus,
        last_sync_outcome: Option<SyncOutcome>,
        last_error: Option<SyncErrorSnapshot>,
    ) -> Self {
        Self {
            network: wallet_status.network,
            driver_status,
            sync_status: wallet_status.sync_status,
            scanned_height: wallet_status.scanned_height,
            safe_chain_tip_height: wallet_status.safe_chain_tip_height,
            lag_blocks: wallet_status.lag_blocks,
            last_sync_outcome,
            last_error,
        }
    }
}

/// Stream of [`SyncSnapshot`] values from a running sync driver.
pub struct SyncSnapshotStream {
    inner: Pin<Box<dyn Stream<Item = SyncSnapshot> + Send>>,
}

impl SyncSnapshotStream {
    fn from_watch(receiver: watch::Receiver<SyncSnapshot>) -> Self {
        Self {
            inner: Box::pin(WatchStream::new(receiver)),
        }
    }

    /// Receives the next snapshot. `None` when the driver has dropped its broadcaster.
    pub async fn next(&mut self) -> Option<SyncSnapshot> {
        self.inner.next().await
    }
}

/// Source-neutral long-lived wallet sync driver.
///
/// The host process owns the Tokio runtime and shutdown policy. `SyncDriver` only owns the
/// wallet catch-up loop: it listens for [`ChainSource::chain_event_envelopes`] when available,
/// falls back to polling, repeatedly calls [`Wallet::sync`] until the observed tip is
/// reached, and publishes [`SyncSnapshot`] values.
pub struct SyncDriver {
    wallet: Wallet,
    chain: Arc<dyn ChainSource>,
    options: SyncDriverOptions,
}

impl SyncDriver {
    /// Constructs a driver for `wallet` and `chain`.
    ///
    /// Fails closed on network mismatch. `not_retryable` on mismatch.
    pub fn new(
        wallet: Wallet,
        chain: Arc<dyn ChainSource>,
        options: SyncDriverOptions,
    ) -> Result<Self, WalletError> {
        if chain.network() != wallet.network() {
            return Err(WalletError::NetworkMismatch {
                storage: wallet.network(),
                requested: chain.network(),
            });
        }
        Ok(Self {
            wallet,
            chain,
            options: options.normalized(),
        })
    }

    /// Starts continuous wallet sync and returns a handle for observation and shutdown.
    #[must_use]
    pub fn sync_continuously(self) -> SyncHandle {
        let (close_tx, close_rx) = oneshot::channel();
        let (status_tx, status_rx) = watch::channel(SyncSnapshot::starting(self.wallet.network()));
        let join = tokio::spawn(run_sync_driver(
            self.wallet,
            self.chain,
            self.options,
            close_rx,
            status_tx,
        ));
        SyncHandle {
            close_tx: Some(close_tx),
            join,
            status_rx,
        }
    }
}

/// Handle returned by [`SyncDriver::sync_continuously`].
pub struct SyncHandle {
    close_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<Result<(), WalletError>>,
    status_rx: watch::Receiver<SyncSnapshot>,
}

impl SyncHandle {
    /// Returns the latest driver snapshot without waiting.
    #[must_use]
    pub fn status_snapshot(&self) -> SyncSnapshot {
        self.status_rx.borrow().clone()
    }

    /// Subscribes to sync-driver snapshots.
    #[must_use]
    pub fn observe_status(&self) -> SyncSnapshotStream {
        SyncSnapshotStream::from_watch(self.status_rx.clone())
    }

    /// Requests shutdown and waits for the driver task to close.
    pub async fn close(mut self) -> Result<(), WalletError> {
        if let Some(close_tx) = self.close_tx.take() {
            let _ = close_tx.send(());
        }
        match self.join.await {
            Ok(join_outcome) => join_outcome,
            Err(join_error) => {
                let posture = if join_error.is_panic() {
                    FailurePosture::RequiresOperator
                } else {
                    FailurePosture::Retryable
                };
                Err(WalletError::SyncDriverFailed {
                    reason: join_error.to_string(),
                    posture,
                })
            }
        }
    }
}

#[derive(Default)]
struct SyncRunState {
    last_sync_outcome: Option<SyncOutcome>,
    last_error: Option<SyncErrorSnapshot>,
}

enum SyncRunAttempt {
    Completed(SyncOutcome),
    RetryableError(SyncErrorSnapshot),
    FatalError {
        error: WalletError,
        snapshot: SyncErrorSnapshot,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncWakeupExit {
    Waiting,
    CloseRequested,
}

async fn run_sync_driver(
    wallet: Wallet,
    chain: Arc<dyn ChainSource>,
    options: SyncDriverOptions,
    mut close_rx: oneshot::Receiver<()>,
    status_tx: watch::Sender<SyncSnapshot>,
) -> Result<(), WalletError> {
    let mut poll = interval(Duration::from_millis(options.poll_interval_ms));
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut chain_event_cursor: Option<ChainEventCursor> = None;
    let mut chain_events = open_chain_events(
        chain.as_ref(),
        &status_tx,
        wallet.network(),
        chain_event_cursor.clone(),
    )
    .await;
    let mut run_state = SyncRunState::default();
    let mut should_sync = true;

    loop {
        if should_sync {
            let mut wakeup_scope = SyncWakeupScope {
                wallet: &wallet,
                chain: chain.as_ref(),
                options,
                status_tx: &status_tx,
                close_rx: &mut close_rx,
            };
            if run_sync_wakeup(&mut wakeup_scope, &mut run_state).await?
                == SyncWakeupExit::CloseRequested
            {
                return close_sync_driver(&wallet, &status_tx, run_state).await;
            }
            should_sync = false;
        }

        tokio::select! {
            _ = &mut close_rx => {
                return close_sync_driver(&wallet, &status_tx, run_state).await;
            }
            _ = poll.tick() => {
                if chain_events.is_none() {
                    chain_events = open_chain_events(
                        chain.as_ref(),
                        &status_tx,
                        wallet.network(),
                        chain_event_cursor.clone(),
                    )
                    .await;
                }
                should_sync = true;
            }
            chain_event = next_chain_event_envelope(&mut chain_events) => {
                match chain_event {
                    Some(Ok(envelope)) => {
                        chain_event_cursor = Some(envelope.cursor);
                        should_sync = true;
                    }
                    Some(Err(err)) => {
                        run_state.last_error = Some(SyncErrorSnapshot {
                            reason: err.to_string(),
                            posture: err.posture(),
                        });
                        chain_events = None;
                        publish_fallback_snapshot(
                            wallet.network(),
                            &status_tx,
                            run_state.last_sync_outcome,
                            run_state.last_error.clone(),
                        );
                    }
                    None => {
                        chain_events = None;
                    }
                }
            }
        }
    }
}

struct SyncWakeupScope<'a> {
    wallet: &'a Wallet,
    chain: &'a dyn ChainSource,
    options: SyncDriverOptions,
    status_tx: &'a watch::Sender<SyncSnapshot>,
    close_rx: &'a mut oneshot::Receiver<()>,
}

async fn close_sync_driver(
    wallet: &Wallet,
    status_tx: &watch::Sender<SyncSnapshot>,
    run_state: SyncRunState,
) -> Result<(), WalletError> {
    publish_driver_snapshot(
        wallet,
        status_tx,
        SyncDriverStatus::Closing,
        run_state.last_sync_outcome,
        run_state.last_error.clone(),
    )
    .await?;
    publish_driver_snapshot(
        wallet,
        status_tx,
        SyncDriverStatus::Closed,
        run_state.last_sync_outcome,
        run_state.last_error,
    )
    .await
}

async fn run_sync_wakeup(
    scope: &mut SyncWakeupScope<'_>,
    run_state: &mut SyncRunState,
) -> Result<SyncWakeupExit, WalletError> {
    for _ in 0..scope.options.max_sync_iterations_per_wake_count {
        tokio::select! {
            biased;
            _ = &mut *scope.close_rx => {
                return Ok(SyncWakeupExit::CloseRequested);
            }
            publish_outcome = publish_driver_snapshot(
                scope.wallet,
                scope.status_tx,
                SyncDriverStatus::Syncing,
                run_state.last_sync_outcome,
                run_state.last_error.clone(),
            ) => {
                publish_outcome?;
            }
        }
        let sync_attempt = tokio::select! {
            biased;
            _ = &mut *scope.close_rx => {
                return Ok(SyncWakeupExit::CloseRequested);
            }
            sync_attempt = run_one_sync(scope.wallet, scope.chain, scope.options) => sync_attempt,
        };
        match sync_attempt {
            SyncRunAttempt::Completed(outcome) => {
                run_state.last_sync_outcome = Some(outcome);
                run_state.last_error = None;
                let wallet_status = scope.wallet.status_snapshot().await?;
                let should_continue = should_continue_syncing(outcome);
                publish_snapshot(
                    scope.status_tx,
                    SyncSnapshot::from_wallet_status(
                        if should_continue {
                            SyncDriverStatus::Syncing
                        } else {
                            SyncDriverStatus::Waiting
                        },
                        &wallet_status,
                        run_state.last_sync_outcome,
                        None,
                    ),
                );
                if !should_continue {
                    return Ok(SyncWakeupExit::Waiting);
                }
            }
            SyncRunAttempt::RetryableError(snapshot) => {
                run_state.last_error = Some(snapshot);
                publish_driver_snapshot(
                    scope.wallet,
                    scope.status_tx,
                    SyncDriverStatus::Waiting,
                    run_state.last_sync_outcome,
                    run_state.last_error.clone(),
                )
                .await?;
                return Ok(SyncWakeupExit::Waiting);
            }
            SyncRunAttempt::FatalError { error, snapshot } => {
                let driver_posture = error.posture();
                run_state.last_error = Some(snapshot);
                publish_driver_snapshot(
                    scope.wallet,
                    scope.status_tx,
                    SyncDriverStatus::Failed {
                        posture: driver_posture,
                    },
                    run_state.last_sync_outcome,
                    run_state.last_error.clone(),
                )
                .await?;
                return Err(error);
            }
        }
    }
    Ok(SyncWakeupExit::Waiting)
}

async fn run_one_sync(
    wallet: &Wallet,
    chain: &dyn ChainSource,
    options: SyncDriverOptions,
) -> SyncRunAttempt {
    match timeout(
        Duration::from_secs(options.sync_timeout_seconds),
        wallet.sync(chain),
    )
    .await
    {
        Ok(Ok(outcome)) => SyncRunAttempt::Completed(outcome),
        Ok(Err(error)) => {
            let posture = error.posture();
            let snapshot = SyncErrorSnapshot {
                reason: error.to_string(),
                posture,
            };
            if posture.allows_retry() {
                SyncRunAttempt::RetryableError(snapshot)
            } else {
                SyncRunAttempt::FatalError { error, snapshot }
            }
        }
        Err(_elapsed) => SyncRunAttempt::RetryableError(SyncErrorSnapshot {
            reason: format!("sync exceeded {} seconds", options.sync_timeout_seconds),
            posture: FailurePosture::Retryable,
        }),
    }
}

/// Whether the driver should run another sync iteration in this wakeup.
///
/// A cycle that scanned a chunk (`block_count > 0`) or rewound a reorg
/// (`reorgs_observed > 0`) leaves more scan-queue work; a caught-up cycle reports neither and
/// stops the loop until the next chain event or poll.
fn should_continue_syncing(outcome: SyncOutcome) -> bool {
    outcome.block_count > 0 || outcome.reorgs_observed > 0
}

async fn publish_driver_snapshot(
    wallet: &Wallet,
    status_tx: &watch::Sender<SyncSnapshot>,
    driver_status: SyncDriverStatus,
    last_sync_outcome: Option<SyncOutcome>,
    last_error: Option<SyncErrorSnapshot>,
) -> Result<(), WalletError> {
    let wallet_status = wallet.status_snapshot().await?;
    publish_snapshot(
        status_tx,
        SyncSnapshot::from_wallet_status(
            driver_status,
            &wallet_status,
            last_sync_outcome,
            last_error,
        ),
    );
    Ok(())
}

fn publish_fallback_snapshot(
    network: Network,
    status_tx: &watch::Sender<SyncSnapshot>,
    last_sync_outcome: Option<SyncOutcome>,
    last_error: Option<SyncErrorSnapshot>,
) {
    let prior = status_tx.borrow().clone();
    publish_snapshot(
        status_tx,
        SyncSnapshot {
            network,
            driver_status: SyncDriverStatus::Waiting,
            sync_status: prior.sync_status,
            scanned_height: prior.scanned_height,
            safe_chain_tip_height: prior.safe_chain_tip_height,
            lag_blocks: prior.lag_blocks,
            last_sync_outcome,
            last_error,
        },
    );
}

fn publish_snapshot(status_tx: &watch::Sender<SyncSnapshot>, snapshot: SyncSnapshot) {
    let _ = status_tx.send(snapshot);
}

async fn open_chain_events(
    chain: &dyn ChainSource,
    status_tx: &watch::Sender<SyncSnapshot>,
    network: Network,
    from_cursor: Option<ChainEventCursor>,
) -> Option<ChainEventEnvelopeStream> {
    match chain.chain_event_envelopes(from_cursor).await {
        Ok(stream) => Some(stream),
        Err(err) => {
            publish_fallback_snapshot(
                network,
                status_tx,
                None,
                Some(SyncErrorSnapshot {
                    reason: err.to_string(),
                    posture: err.posture(),
                }),
            );
            None
        }
    }
}

async fn next_chain_event_envelope(
    chain_events: &mut Option<ChainEventEnvelopeStream>,
) -> Option<Result<ChainEventEnvelope, ChainSourceError>> {
    match chain_events {
        Some(stream) => stream.next().await,
        None => future::pending().await,
    }
}

impl Wallet {
    /// Advances the wallet by one bounded scan step toward `chain.chain_tip()`.
    ///
    /// Each call primes commitment-tree subtree roots, calls `update_chain_tip` with the live
    /// tip, then scans the highest-priority range `suggest_scan_ranges` returns (chunked to
    /// [`MAX_BLOCKS_PER_SYNC`]). The `SyncDriver` loops until the scan queue drains. Subtree
    /// roots let the wallet witness a note from its subtree root without scanning every block,
    /// so spendability does not require a full linear scan; transaction expiry heights are
    /// computed against the live tip. Reorg safety comes from the spend-time confirmation depth
    /// (ZIP 315) and from in-loop recovery: a `ChainReorgDetected` triggers a precise rewind
    /// via `truncate_to_chain_state` and a re-plan on the next tick. Divergences deeper than the
    /// librustzcash 100-block rewind cap (`COINBASE_MATURITY`) surface as `RequiresOperator`.
    ///
    /// Fails closed on network mismatch. Emits `ScanProgress` events at the start and end of
    /// the run; per-block events are emitted by the storage scanner.
    ///
    /// `not_retryable` on network mismatch. `retryable` on transient chain-source failures.
    pub async fn sync(&self, chain: &dyn ChainSource) -> Result<SyncOutcome, WalletError> {
        let outcome = self.sync_inner(chain).await?;
        self.retire_expired_pending_broadcasts().await?;
        Ok(outcome)
    }

    async fn retire_expired_pending_broadcasts(&self) -> Result<(), WalletError> {
        let before_at_ms = crate::wallet::current_unix_ms()
            .saturating_sub(self.inner.options.pending_broadcast_window_ms);
        self.inner
            .storage
            .clear_expired_pending_broadcast_inputs(before_at_ms)
            .await?;
        Ok(())
    }

    async fn sync_inner(&self, chain: &dyn ChainSource) -> Result<SyncOutcome, WalletError> {
        if chain.network() != self.network() {
            return Err(WalletError::NetworkMismatch {
                storage: self.network(),
                requested: chain.network(),
            });
        }
        let policy = self.retry_policy();
        let chain_tip = with_breaker_and_retry(
            &self.inner.circuit_breaker,
            policy,
            "sync.chain_tip",
            || chain.chain_tip(),
            WalletError::from,
        )
        .await?;
        self.inner.storage.record_observed_tip(chain_tip).await?;

        let Some((scan_start, scan_end, priority)) =
            self.plan_scan_range(chain, chain_tip).await?
        else {
            let transparent_utxo_count = self.sync_transparent_utxos(chain).await?;
            return Ok(self.emit_caught_up(chain_tip, transparent_utxo_count));
        };
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: scan_start,
            target_height: chain_tip,
        });
        tracing::info!(
            target: "zally::sync",
            event = "wallet_sync_cycle",
            scanned_from = scan_start.as_u32(),
            scan_end = scan_end.as_u32(),
            chain_tip = chain_tip.as_u32(),
            priority,
            "sync cycle: scanning a suggested range chunk"
        );

        let from_state = fetch_prior_chain_state(chain, scan_start).await?;
        let blocks = fetch_compact_blocks(chain, scan_start, scan_end).await?;
        let block_count = u64::try_from(blocks.len()).unwrap_or(u64::MAX);
        tracing::info!(
            target: "zally::sync",
            event = "wallet_sync_fetched",
            scanned_from = scan_start.as_u32(),
            scan_end = scan_end.as_u32(),
            block_count,
            "fetched compact blocks for scan"
        );
        if blocks.is_empty() {
            tracing::warn!(
                target: "zally::sync",
                event = "wallet_sync_empty_fetch",
                scanned_from = scan_start.as_u32(),
                scan_end = scan_end.as_u32(),
                "suggested-range fetch returned no blocks"
            );
            let transparent_utxo_count = self.sync_transparent_utxos(chain).await?;
            return Ok(self.emit_caught_up(chain_tip, transparent_utxo_count));
        }

        match self
            .scan_and_emit(
                ScanContext {
                    blocks,
                    scanned_from: scan_start,
                    target_height: chain_tip,
                    block_count,
                },
                from_state,
                chain,
            )
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(WalletError::Storage(zally_storage::StorageError::ChainReorgDetected {
                at_height,
            })) => self.recover_from_reorg(chain, at_height).await,
            Err(other) => Err(other),
        }
    }

    /// Resolves the next scan range, advancing the chain tip only when the queue is drained.
    ///
    /// Each `update_chain_tip` call re-creates a short `Verify` range (`VERIFY_LOOKAHEAD`
    /// blocks) at the scan frontier while the wallet is far behind, so calling it every cycle
    /// would force catch-up into 10-block steps. This drains the existing queue first and
    /// only advances the tip, re-priming subtree roots, once the queue is empty and the live
    /// tip is ahead. That matches the library recipe: update the tip once, then scan all
    /// suggested ranges (one `Verify`, then bulk `Historic`/`ChainTip`) before touching it
    /// again.
    async fn plan_scan_range(
        &self,
        chain: &dyn ChainSource,
        chain_tip: BlockHeight,
    ) -> Result<Option<(BlockHeight, BlockHeight, &'static str)>, WalletError> {
        if let Some(range) = self.next_scan_range(chain_tip).await? {
            return Ok(Some(range));
        }
        let fully_scanned = self.inner.storage.fully_scanned_height().await?;
        if fully_scanned.is_none_or(|h| chain_tip.as_u32() > h.as_u32()) {
            self.backfill_subtree_roots(chain).await?;
            self.inner.storage.update_chain_tip(chain_tip).await?;
            self.next_scan_range(chain_tip).await
        } else {
            Ok(None)
        }
    }

    /// Returns the highest-priority suggested scan range that lies at or below `chain_tip`,
    /// chunked to at most [`MAX_BLOCKS_PER_SYNC`] blocks, as `(start, end_inclusive,
    /// priority_label)`. `None` when nothing at or below the tip remains to scan.
    ///
    /// Ranges are clamped to `chain_tip`: a suggested range can start above the tip when the
    /// wallet birthday is ahead of the chain (the chain has not reached it yet), and a range
    /// can extend past the tip if the queue was planned against a higher tip; neither is
    /// fetchable, so both are skipped or trimmed.
    async fn next_scan_range(
        &self,
        chain_tip: BlockHeight,
    ) -> Result<Option<(BlockHeight, BlockHeight, &'static str)>, WalletError> {
        let tip = chain_tip.as_u32();
        for range in self.inner.storage.suggest_scan_ranges().await? {
            if range.is_empty() {
                continue;
            }
            let block_range = range.block_range();
            let start = u32::from(block_range.start);
            if start > tip {
                continue;
            }
            let end_inclusive = u32::from(block_range.end).saturating_sub(1).min(tip);
            let chunk_end = start
                .saturating_add(MAX_BLOCKS_PER_SYNC.saturating_sub(1))
                .min(end_inclusive);
            return Ok(Some((
                BlockHeight::from(start),
                BlockHeight::from(chunk_end),
                scan_priority_label(range.priority()),
            )));
        }
        Ok(None)
    }

    /// Fetches and records every new subtree root for both shielded pools.
    ///
    /// Idempotent: re-recording a root the wallet already holds is a no-op, so this runs from
    /// index 0 each cycle and stops at the first short page.
    async fn backfill_subtree_roots(&self, chain: &dyn ChainSource) -> Result<(), WalletError> {
        for (pool, protocol) in [
            (ShieldedPool::Sapling, zcash_protocol::ShieldedProtocol::Sapling),
            (ShieldedPool::Orchard, zcash_protocol::ShieldedProtocol::Orchard),
        ] {
            let mut next_index = 0_u32;
            loop {
                let policy = self.retry_policy();
                let roots = with_breaker_and_retry(
                    &self.inner.circuit_breaker,
                    policy,
                    "sync.subtree_roots",
                    || chain.subtree_roots(pool, SubtreeIndex(next_index), SUBTREE_ROOT_PAGE),
                    WalletError::from,
                )
                .await?;
                let (Some(first), Some(last)) = (roots.first(), roots.last()) else {
                    break;
                };
                let start_index = u64::from(first.index.0);
                let last_index = last.index.0;
                let page_len = roots.len();
                let entries: Vec<(BlockHeight, [u8; 32])> = roots
                    .into_iter()
                    .map(|root| (root.completing_block_height, root.root_bytes))
                    .collect();
                self.inner
                    .storage
                    .put_subtree_roots(protocol, start_index, entries)
                    .await?;
                if page_len < SUBTREE_ROOT_PAGE as usize {
                    break;
                }
                next_index = last_index.saturating_add(1);
            }
        }
        Ok(())
    }

    /// Recovers from a scan-time chain divergence by rewinding to the exact prior chain state.
    ///
    /// Fetches the chain state at `at_height - REORG_REWIND_BLOCKS` from the chain source and
    /// truncates the wallet precisely to it (via [`WalletStorage::truncate_to_chain_state`]),
    /// then returns an outcome that re-triggers a sync tick so `suggest_scan_ranges` re-plans
    /// from the rewound frontier.
    ///
    /// `not_retryable` if the rewind target is below the librustzcash 100-block
    /// `COINBASE_MATURITY` cap; the operator must reset the wallet.
    async fn recover_from_reorg(
        &self,
        chain: &dyn ChainSource,
        at_height: BlockHeight,
    ) -> Result<SyncOutcome, WalletError> {
        let rewind_to = BlockHeight::from(at_height.as_u32().saturating_sub(REORG_REWIND_BLOCKS));
        tracing::warn!(
            target: "zally::sync",
            event = "wallet_sync_reorg_recover",
            at_height = at_height.as_u32(),
            rewind_to = rewind_to.as_u32(),
            "scan-time reorg detected; rewinding to the exact prior chain state"
        );
        let chain_state = chain_state_at(chain, rewind_to).await?;
        self.inner
            .storage
            .truncate_to_chain_state(chain_state)
            .await?;
        self.publish_event(WalletEvent::ReorgDetected {
            rolled_back_to_height: rewind_to,
            new_safe_chain_tip_height: rewind_to,
        });
        Ok(SyncOutcome {
            scanned_from_height: rewind_to,
            scanned_to_height: rewind_to,
            block_count: 0,
            transparent_utxo_count: 0,
            reorgs_observed: 1,
        })
    }

    fn emit_caught_up(&self, target_height: BlockHeight, transparent_utxo_count: u64) -> SyncOutcome {
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: target_height,
            target_height,
        });
        SyncOutcome {
            scanned_from_height: target_height,
            scanned_to_height: target_height,
            block_count: 0,
            transparent_utxo_count,
            reorgs_observed: 0,
        }
    }

    async fn scan_and_emit(
        &self,
        context: ScanContext,
        from_state: ChainState,
        chain: &dyn ChainSource,
    ) -> Result<SyncOutcome, WalletError> {
        let ScanContext {
            blocks,
            scanned_from,
            target_height,
            block_count,
        } = context;
        let timestamps_by_height = block_timestamp_index(&blocks);
        let outcome = self
            .inner
            .storage
            .scan_blocks(ScanRequest::new(blocks, scanned_from, from_state))
            .await?;

        let newly_confirmed = self
            .inner
            .storage
            .wallet_tx_ids_mined_in_range(scanned_from, outcome.scanned_to_height)
            .await?;
        self.retire_pending_broadcasts_for_mined(&newly_confirmed)
            .await?;
        for (tx_id, confirmed_at_height) in newly_confirmed {
            self.publish_event(WalletEvent::TransactionConfirmed {
                tx_id,
                confirmed_at_height,
            });
        }

        let received_notes = self
            .inner
            .storage
            .received_shielded_notes_mined_in_range(scanned_from, outcome.scanned_to_height)
            .await?;
        for note in received_notes {
            let block_timestamp_ms = if note.block_timestamp_ms != 0 {
                note.block_timestamp_ms
            } else {
                timestamps_by_height
                    .get(&note.mined_height.as_u32())
                    .copied()
                    .unwrap_or(0)
            };
            self.publish_event(WalletEvent::ShieldedReceiveObserved {
                account_id: note.account_id,
                tx_id: note.tx_id,
                output_index: note.output_index,
                value_zat: note.value_zat,
                mined_height: note.mined_height,
                block_timestamp_ms,
                pool: shielded_pool_for(note.protocol),
                is_change: note.is_change,
                spent_our_inputs: note.spent_our_inputs,
            });
        }

        let transparent_utxo_count = self.sync_transparent_utxos(chain).await?;

        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: outcome.scanned_to_height,
            target_height,
        });
        Ok(SyncOutcome {
            scanned_from_height: scanned_from,
            scanned_to_height: outcome.scanned_to_height,
            block_count,
            transparent_utxo_count,
            reorgs_observed: 0,
        })
    }

    async fn retire_pending_broadcasts_for_mined(
        &self,
        newly_confirmed: &[(zally_core::TxId, BlockHeight)],
    ) -> Result<(), WalletError> {
        if newly_confirmed.is_empty() {
            return Ok(());
        }
        let confirmed_tx_ids: Vec<_> = newly_confirmed.iter().map(|(tx_id, _)| *tx_id).collect();
        self.inner
            .storage
            .clear_pending_broadcast_inputs_for_mined(&confirmed_tx_ids)
            .await?;
        Ok(())
    }

    async fn sync_transparent_utxos(&self, chain: &dyn ChainSource) -> Result<u64, WalletError> {
        let receivers = self.inner.storage.list_transparent_receivers().await?;
        let mut transparent_utxo_count = 0_u64;
        for receiver in receivers {
            let policy = self.retry_policy();
            let utxos = with_breaker_and_retry(
                &self.inner.circuit_breaker,
                policy,
                "sync.transparent_utxos",
                || chain.transparent_utxos(&receiver.script_pub_key_bytes),
                WalletError::from,
            )
            .await?;

            let mut transparent_utxo_rows = Vec::with_capacity(utxos.len());
            for utxo in utxos {
                if utxo.script_pub_key_bytes != receiver.script_pub_key_bytes {
                    return Err(WalletError::ChainSource(
                        ChainSourceError::MalformedCompactBlock {
                            block_height: utxo.confirmed_at_height,
                            reason: format!(
                                "transparent UTXO script did not match wallet receiver for account {:?}",
                                receiver.account_id
                            ),
                        },
                    ));
                }
                let value_zat = zally_core::Zatoshis::try_from(utxo.value_zat).map_err(|_| {
                    WalletError::ChainSource(ChainSourceError::MalformedCompactBlock {
                        block_height: utxo.confirmed_at_height,
                        reason: format!(
                            "transparent UTXO value {} exceeds MAX_MONEY for account {:?}",
                            utxo.value_zat, receiver.account_id
                        ),
                    })
                })?;
                transparent_utxo_rows.push(TransparentUtxoRow::new(
                    utxo.tx_id,
                    utxo.output_index,
                    value_zat,
                    utxo.confirmed_at_height,
                    utxo.script_pub_key_bytes,
                ));
            }
            transparent_utxo_count = transparent_utxo_count.saturating_add(
                self.inner
                    .storage
                    .record_transparent_utxos(transparent_utxo_rows)
                    .await?,
            );
        }
        Ok(transparent_utxo_count)
    }
}

fn block_timestamp_index(
    blocks: &[zcash_client_backend::proto::compact_formats::CompactBlock],
) -> HashMap<u32, u64> {
    blocks
        .iter()
        .map(|block| {
            let height = u32::try_from(block.height).unwrap_or(u32::MAX);
            let timestamp_ms = u64::from(block.time).saturating_mul(1_000);
            (height, timestamp_ms)
        })
        .collect()
}

const fn shielded_pool_for(protocol: zcash_protocol::ShieldedProtocol) -> ShieldedPool {
    match protocol {
        zcash_protocol::ShieldedProtocol::Sapling => ShieldedPool::Sapling,
        zcash_protocol::ShieldedProtocol::Orchard => ShieldedPool::Orchard,
    }
}

const fn scan_priority_label(priority: ScanPriority) -> &'static str {
    match priority {
        ScanPriority::Ignored => "ignored",
        ScanPriority::Scanned => "scanned",
        ScanPriority::Historic => "historic",
        ScanPriority::OpenAdjacent => "open_adjacent",
        ScanPriority::FoundNote => "found_note",
        ScanPriority::ChainTip => "chain_tip",
        ScanPriority::Verify => "verify",
    }
}

async fn fetch_compact_blocks(
    chain: &dyn ChainSource,
    start_height: BlockHeight,
    end_height: BlockHeight,
) -> Result<Vec<zcash_client_backend::proto::compact_formats::CompactBlock>, WalletError> {
    let range = BlockHeightRange {
        start_height,
        end_height,
    };
    let mut stream = chain.compact_blocks(range).await?;
    let mut blocks = Vec::new();
    while let Some(stream_item) = stream.next().await {
        blocks.push(stream_item?);
    }
    Ok(blocks)
}

/// Fetches the `ChainState` at exactly `height` (the note-commitment frontier after `height`).
///
/// Returns a [`ChainSourceError::MalformedCompactBlock`] when the tree-state bytes cannot be
/// decoded.
pub(crate) async fn chain_state_at(
    chain: &dyn ChainSource,
    height: BlockHeight,
) -> Result<ChainState, WalletError> {
    let tree_state = chain.tree_state_at(height).await?;
    tree_state.to_chain_state().map_err(|io| {
        WalletError::ChainSource(ChainSourceError::MalformedCompactBlock {
            block_height: height,
            reason: format!("invalid tree state: {io}"),
        })
    })
}

/// Fetches the `ChainState` anchor immediately below `at_height`.
///
/// Shared by `sync_inner` (the `from_state` for a scan range) and the wallet builder (for
/// birthday). The chain source serves the tree state at the exact prior height.
pub(crate) async fn fetch_prior_chain_state(
    chain: &dyn ChainSource,
    at_height: BlockHeight,
) -> Result<ChainState, WalletError> {
    chain_state_at(
        chain,
        BlockHeight::from(at_height.as_u32().saturating_sub(1)),
    )
    .await
}

