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
    ChainSourceError, ChainState, ShieldedPool,
};
use zally_core::{BlockHeight, Network};
use zally_storage::{ScanRequest, StorageError, TransparentUtxoRow};

use crate::event::WalletEvent;
use crate::retry::with_breaker_and_retry;
use crate::status::{SyncStatus, WalletStatus};
use crate::wallet::Wallet;
use crate::wallet_error::WalletError;

const MAX_BLOCKS_PER_SYNC: u32 = 1_000;

/// Blocks to roll back past a divergence reported by `scan_cached_blocks`.
///
/// Must exceed the chain source's reorg window so recovery always makes forward progress,
/// even when the divergence is detected at `fully_scanned_height + 1` (where an
/// `at_height - 1` rollback would be a no-op). 160 blocks leaves comfortable margin above
/// typical Zcash reorg windows (100 blocks for shielded coinbase maturity, lower in
/// practice on testnet and regtest).
const REORG_ROLLBACK_DEPTH_BLOCKS: u32 = 160;

struct ScanContext {
    blocks: Vec<zcash_client_backend::proto::compact_formats::CompactBlock>,
    scanned_from: BlockHeight,
    target_height: BlockHeight,
    block_count: u64,
    reorgs_observed: u32,
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
    /// The driver stopped after a non-retryable error.
    Failed {
        /// Whether the failure may succeed if the caller starts a new driver.
        is_retryable: bool,
    },
}

/// Cloneable error summary carried by [`SyncSnapshot`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncErrorSnapshot {
    /// Error description safe for logs and status pages.
    pub reason: String,
    /// Whether retrying the same operation may succeed.
    pub is_retryable: bool,
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
    pub chain_tip_height: Option<BlockHeight>,
    /// Number of blocks between `scanned_height` and `chain_tip_height`, if known.
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
            chain_tip_height: None,
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
            chain_tip_height: wallet_status.chain_tip_height,
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
/// wallet catch-up loop: it listens for [`ChainSource::chain_events`] when available,
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
            Err(join_error) => Err(WalletError::SyncDriverFailed {
                reason: join_error.to_string(),
                is_retryable: !join_error.is_panic(),
            }),
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
                            is_retryable: err.is_retryable(),
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
                let should_continue = should_continue_syncing(outcome, &wallet_status);
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
                run_state.last_error = Some(snapshot);
                publish_driver_snapshot(
                    scope.wallet,
                    scope.status_tx,
                    SyncDriverStatus::Failed {
                        is_retryable: false,
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
        Ok(Err(error)) if error.is_retryable() => {
            SyncRunAttempt::RetryableError(SyncErrorSnapshot {
                reason: error.to_string(),
                is_retryable: true,
            })
        }
        Ok(Err(error)) => {
            let snapshot = SyncErrorSnapshot {
                reason: error.to_string(),
                is_retryable: false,
            };
            SyncRunAttempt::FatalError { error, snapshot }
        }
        Err(_elapsed) => SyncRunAttempt::RetryableError(SyncErrorSnapshot {
            reason: format!("sync exceeded {} seconds", options.sync_timeout_seconds),
            is_retryable: true,
        }),
    }
}

fn should_continue_syncing(outcome: SyncOutcome, wallet_status: &WalletStatus) -> bool {
    if outcome.reorgs_observed > 0 {
        return true;
    }
    let Some(chain_tip_height) = wallet_status.chain_tip_height else {
        return false;
    };
    outcome.block_count > 0 && outcome.scanned_to_height.as_u32() < chain_tip_height.as_u32()
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
            chain_tip_height: prior.chain_tip_height,
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
                    is_retryable: err.is_retryable(),
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
    /// Advances the wallet from its last-scanned height up to `chain.chain_tip()`.
    ///
    /// Scanning reaches the chain tip so the commitment tree, note witnesses, and the
    /// `WalletDb` chain-tip notion all agree: `zcash_client_backend` only treats a note as
    /// spendable when its witness is anchored within a fully-scanned tip, and transaction
    /// expiry heights are computed against that same tip. Reorg safety comes from the
    /// spend-time confirmation depth (ZIP 315) and from `roll_back_after_reorg` recovery, not
    /// from withholding recent blocks from the scan.
    ///
    /// Fails closed on network mismatch. Emits `ScanProgress` events at the start and end of
    /// the run; per-block events are emitted by the storage scanner.
    ///
    /// `not_retryable` on network mismatch. `retryable` on transient chain-source failures.
    pub async fn sync(&self, chain: &dyn ChainSource) -> Result<SyncOutcome, WalletError> {
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
            |e| map_chain_source_error(&e),
        )
        .await?;
        let prior_observed_tip = self.inner.storage.lookup_observed_tip().await?;
        let reorg = self.detect_tip_regress(prior_observed_tip, chain_tip);
        self.inner.storage.record_observed_tip(chain_tip).await?;
        // Pin the WalletDb chain tip to the height this run scans to, so transaction
        // proposals compute expiry and anchor heights against a fully-scanned tip.
        self.inner.storage.update_chain_tip(chain_tip).await?;

        let prior_fully_scanned_height = self.inner.storage.fully_scanned_height().await?;
        let scanned_from = match prior_fully_scanned_height {
            Some(h) => BlockHeight::from(h.as_u32().saturating_add(1)),
            None => self
                .inner
                .storage
                .wallet_birthday()
                .await?
                .unwrap_or_else(|| BlockHeight::from(1)),
        };
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: scanned_from,
            target_height: chain_tip,
        });

        if scanned_from.as_u32() > chain_tip.as_u32() {
            let transparent_utxo_count = self.sync_transparent_utxos(chain).await?;
            return Ok(self.emit_already_caught_up(
                scanned_from,
                chain_tip,
                reorg,
                transparent_utxo_count,
            ));
        }
        let blocks = fetch_compact_blocks(chain, scanned_from, chain_tip).await?;
        let block_count = u64::try_from(blocks.len()).unwrap_or(u64::MAX);
        if blocks.is_empty() {
            let transparent_utxo_count = self.sync_transparent_utxos(chain).await?;
            return Ok(self.emit_already_caught_up(
                scanned_from,
                chain_tip,
                reorg,
                transparent_utxo_count,
            ));
        }
        let from_state = fetch_prior_chain_state(chain, scanned_from).await?;
        self.scan_and_emit(
            ScanContext {
                blocks,
                scanned_from,
                target_height: chain_tip,
                block_count,
                reorgs_observed: reorg,
            },
            from_state,
            chain,
        )
        .await
    }

    fn detect_tip_regress(
        &self,
        prior_observed_tip: Option<BlockHeight>,
        new_tip_height: BlockHeight,
    ) -> u32 {
        let Some(prior) = prior_observed_tip else {
            return 0;
        };
        if new_tip_height.as_u32() >= prior.as_u32() {
            return 0;
        }
        self.publish_event(WalletEvent::ReorgDetected {
            rolled_back_to_height: new_tip_height,
            new_tip_height,
        });
        1
    }

    fn emit_already_caught_up(
        &self,
        scanned_from: BlockHeight,
        target_height: BlockHeight,
        reorgs_observed: u32,
        transparent_utxo_count: u64,
    ) -> SyncOutcome {
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: target_height,
            target_height,
        });
        SyncOutcome {
            scanned_from_height: scanned_from,
            scanned_to_height: target_height,
            block_count: 0,
            transparent_utxo_count,
            reorgs_observed,
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
            reorgs_observed,
        } = context;
        let timestamps_by_height = block_timestamp_index(&blocks);
        let outcome = match self
            .inner
            .storage
            .scan_blocks(ScanRequest::new(blocks, scanned_from, from_state))
            .await
        {
            Ok(outcome) => outcome,
            Err(StorageError::ChainReorgDetected { at_height }) => {
                return self
                    .roll_back_after_reorg(at_height, target_height, reorgs_observed)
                    .await;
            }
            Err(other) => return Err(WalletError::from(other)),
        };

        let newly_confirmed = self
            .inner
            .storage
            .wallet_tx_ids_mined_in_range(scanned_from, outcome.scanned_to_height)
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
            reorgs_observed,
        })
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
                |e| map_chain_source_error(&e),
            )
            .await?;

            let mut transparent_utxo_rows = Vec::with_capacity(utxos.len());
            for utxo in utxos {
                if utxo.script_pub_key_bytes != receiver.script_pub_key_bytes {
                    return Err(WalletError::ChainSource {
                        reason: format!(
                            "transparent UTXO script did not match wallet receiver for account {:?}",
                            receiver.account_id
                        ),
                        is_retryable: false,
                    });
                }
                transparent_utxo_rows.push(TransparentUtxoRow::new(
                    utxo.tx_id,
                    utxo.output_index,
                    utxo.value_zat,
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

impl Wallet {
    /// Recovers from a chain-source-side reorg observed during scan.
    ///
    /// `scan_cached_blocks` rejected the batch because the parent hash of the next block did
    /// not match the wallet's view at `at_height`. Truncate the wallet back by a full reorg
    /// window below the divergence (floored at the wallet birthday), emit `ReorgDetected`,
    /// and return a no-op outcome for this sync. The next sync iteration re-fetches from the
    /// new fully-scanned height, pulls fresh `ChainState` from the chain source, and resumes.
    ///
    /// Rolling back a window rather than a single block is what guarantees forward progress:
    /// `scan_cached_blocks` reports the divergence at `fully_scanned_height + 1`, so an
    /// `at_height - 1` rollback leaves the wallet at the same scan point and retries the same
    /// invalid range.
    async fn roll_back_after_reorg(
        &self,
        at_height: BlockHeight,
        target_height: BlockHeight,
        prior_reorgs: u32,
    ) -> Result<SyncOutcome, WalletError> {
        let birthday = self
            .inner
            .storage
            .wallet_birthday()
            .await?
            .unwrap_or(BlockHeight::GENESIS);
        let rollback_to = reorg_rollback_target(at_height, birthday);
        let new_fully_scanned = self.inner.storage.truncate_to_height(rollback_to).await?;
        tracing::warn!(
            target: "zally::wallet::sync",
            event = "wallet_reorg_recovered",
            at_height = at_height.as_u32(),
            rolled_back_to_height = new_fully_scanned.as_u32(),
            target_height = target_height.as_u32(),
            "rolled wallet back after chain reorg; next sync will re-fetch"
        );
        self.publish_event(WalletEvent::ReorgDetected {
            rolled_back_to_height: new_fully_scanned,
            new_tip_height: target_height,
        });
        Ok(SyncOutcome {
            scanned_from_height: new_fully_scanned,
            scanned_to_height: new_fully_scanned,
            block_count: 0,
            transparent_utxo_count: 0,
            reorgs_observed: prior_reorgs.saturating_add(1),
        })
    }
}

/// Computes the height to truncate the wallet to after `scan_cached_blocks` reports a
/// divergence at `at_height`.
///
/// Rolls back a full reorg window below the divergence so the next sync re-fetches a fresh
/// range and makes forward progress: `scan_cached_blocks` reports the divergence at
/// `fully_scanned_height + 1`, so a single-block rollback would leave the wallet exactly
/// where it was. Floored at the wallet birthday so a deep-height wallet never re-scans from
/// genesis.
fn reorg_rollback_target(at_height: BlockHeight, birthday: BlockHeight) -> BlockHeight {
    at_height
        .saturating_sub(REORG_ROLLBACK_DEPTH_BLOCKS)
        .max(birthday)
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

async fn fetch_compact_blocks(
    chain: &dyn ChainSource,
    scanned_from: BlockHeight,
    target_height: BlockHeight,
) -> Result<Vec<zcash_client_backend::proto::compact_formats::CompactBlock>, WalletError> {
    let span_end = scanned_from
        .as_u32()
        .saturating_add(MAX_BLOCKS_PER_SYNC.saturating_sub(1))
        .min(target_height.as_u32());
    let range = BlockHeightRange {
        start_height: scanned_from,
        end_height: BlockHeight::from(span_end),
    };
    let mut stream = chain
        .compact_blocks(range)
        .await
        .map_err(|e| map_chain_source_error(&e))?;
    let mut blocks = Vec::new();
    while let Some(stream_item) = stream.next().await {
        let block = stream_item.map_err(|e| map_chain_source_error(&e))?;
        blocks.push(block);
    }
    Ok(blocks)
}

fn map_chain_source_error(err: &ChainSourceError) -> WalletError {
    WalletError::ChainSource {
        reason: err.to_string(),
        is_retryable: err.is_retryable(),
    }
}

async fn fetch_prior_chain_state(
    chain: &dyn ChainSource,
    scanned_from: BlockHeight,
) -> Result<ChainState, WalletError> {
    let prior_height = BlockHeight::from(scanned_from.as_u32().saturating_sub(1));
    let tree_state = chain
        .tree_state_at(prior_height)
        .await
        .map_err(|e| map_chain_source_error(&e))?;
    tree_state
        .to_chain_state()
        .map_err(|io| WalletError::ChainSource {
            reason: format!(
                "invalid tree state at height {}: {io}",
                prior_height.as_u32()
            ),
            is_retryable: false,
        })
}

#[cfg(test)]
mod tests {
    use super::{REORG_ROLLBACK_DEPTH_BLOCKS, reorg_rollback_target};
    use zally_core::BlockHeight;

    #[test]
    fn rollback_target_lands_a_window_below_a_boundary_divergence() {
        // `scan_cached_blocks` reports the divergence at `fully_scanned_height + 1`.
        // The rollback must land strictly below `fully_scanned_height` so the next sync
        // re-fetches a fresh range instead of retrying the same invalid block.
        let fully_scanned_height = 4_009_770;
        let at_height = BlockHeight::from(fully_scanned_height + 1);
        let birthday = BlockHeight::from(4_009_000);
        let target = reorg_rollback_target(at_height, birthday);
        assert!(
            target.as_u32() < fully_scanned_height,
            "rollback target {} must be below fully_scanned_height {fully_scanned_height}",
            target.as_u32(),
        );
        assert_eq!(
            target.as_u32(),
            at_height.as_u32() - REORG_ROLLBACK_DEPTH_BLOCKS,
        );
    }

    #[test]
    fn rollback_target_is_floored_at_birthday() {
        // A divergence within the rollback window of the birthday must not roll back below it.
        let at_height = BlockHeight::from(4_009_010);
        let birthday = BlockHeight::from(4_009_000);
        assert_eq!(reorg_rollback_target(at_height, birthday), birthday);
    }

    #[test]
    fn rollback_target_saturates_to_birthday_floor() {
        // A low-height divergence saturates at the birthday rather than underflowing.
        let at_height = BlockHeight::from(50);
        let birthday = BlockHeight::from(1);
        assert_eq!(reorg_rollback_target(at_height, birthday), birthday);
    }
}
