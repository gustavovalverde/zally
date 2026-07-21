//! Wallet sync loop.
//!
//! `Wallet::sync` pins one source [`zally_chain::ChainEpoch`], selects a bounded scan range no
//! higher than its visible tip, and requests the exact predecessor [`TreeStateArtifact`]. The
//! chain source streams source-neutral compact blocks; the wallet validates their epoch and
//! range, then hands the blocks and anchor to [`zally_storage::WalletStorage::scan_blocks`]. The
//! storage boundary alone translates those values into librustzcash scan types and updates the
//! live wallet database.
//!
//! Successive calls advance through bounded chunks from the wallet's last fully scanned height.
//! The exact predecessor tree artifact anchors each chunk, so work is proportional to the new
//! scan range rather than the full chain height.
//!
//! The long-lived [`SyncDriver`] wraps the loop in a self-healing lifecycle. Wallet chain
//! state is disposable derived state: every fault is classified onto an escalating repair
//! ladder ([`SyncRepair`]) that retries, rewinds below the divergence, rebuilds from the
//! seed and birthday, or parks when no software action cures it. The driver task is
//! infallible while its handle is alive; it exits only through [`SyncHandle::close`].

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt as _, future};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, sleep, timeout};
use tokio_stream::wrappers::WatchStream;
use zally_chain::{
    BlockHeightRange, ChainEventCursorRecovery, ChainEventEnvelope, ChainEventEnvelopeStream,
    ChainEventStreamStart, ChainSource, ChainSourceError, FailurePosture, ShieldedPool,
    SubtreeIndex, TransparentUtxo,
};
use zally_core::{BlockHeight, CompactBlockArtifact, Network, TreeStateArtifact};
use zally_storage::{ScanRequest, StorageError, TransparentReceiverRow, TransparentUtxoRow};
use zcash_client_backend::data_api::scanning::ScanPriority;

use crate::error::WalletError;
use crate::event::WalletEvent;
use crate::retry::with_breaker_and_retry;
use crate::status::{SyncStatus, WalletStatus};
use crate::wallet::{Wallet, current_unix_ms};

/// Maximum compact blocks scanned in one `Wallet::sync` call. A suggested range larger than
/// this is scanned across successive calls; the driver loops until the scan queue drains.
const MAX_BLOCKS_PER_SYNC: u32 = 1_000;

/// Subtree-root page size for the per-cycle backfill. Zinder clamps to its own page cap.
const SUBTREE_ROOT_PAGE: u32 = 128;

/// Rewind depths the repair ladder walks before escalating to a rebuild.
///
/// The deepest rung rewinds 100 blocks: nodes never apply a reorg deeper than coinbase
/// maturity minus one (both zcashd and zebra enforce the cap), so a 100-block rewind clears
/// any divergence the chain can serve. Deeper rewinds are pointless; the next rung is a
/// rebuild from the birthday.
const REWIND_LADDER_BLOCKS: [u32; 2] = [10, 100];

struct ScanContext {
    blocks: Vec<CompactBlockArtifact>,
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
    /// Unix milliseconds when this run completed.
    pub completed_at_ms: u64,
}

/// Self-healing policy for the [`SyncDriver`] repair ladder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncRecoveryPolicy {
    /// Consecutive faults at one ladder rung before the driver escalates to the next rung.
    /// Within [`SyncRepair::Rewind`] the same counter walks the rewind depth ladder.
    pub escalate_after_faults: u32,
    /// Rebuilds from the birthday attempted before the driver parks.
    pub max_rescan_attempts: u32,
    /// Backoff before the first faulted re-attempt, in milliseconds. Doubles per
    /// consecutive fault.
    pub fault_backoff_initial_ms: u64,
    /// Cap on the fault backoff, in milliseconds.
    pub fault_backoff_cap_ms: u64,
    /// How long a parked driver holds before re-arming the full ladder, in milliseconds.
    /// `None` parks forever; the driver keeps republishing its reason either way.
    pub park_reprobe_ms: Option<u64>,
}

impl SyncRecoveryPolicy {
    /// Returns the policy with `escalate_after_faults` replaced.
    #[must_use]
    pub const fn with_escalate_after_faults(self, escalate_after_faults: u32) -> Self {
        Self {
            escalate_after_faults,
            ..self
        }
    }

    /// Returns the policy with `max_rescan_attempts` replaced.
    #[must_use]
    pub const fn with_max_rescan_attempts(self, max_rescan_attempts: u32) -> Self {
        Self {
            max_rescan_attempts,
            ..self
        }
    }

    /// Returns the policy with `fault_backoff_initial_ms` replaced.
    #[must_use]
    pub const fn with_fault_backoff_initial_ms(self, fault_backoff_initial_ms: u64) -> Self {
        Self {
            fault_backoff_initial_ms,
            ..self
        }
    }

    /// Returns the policy with `fault_backoff_cap_ms` replaced.
    #[must_use]
    pub const fn with_fault_backoff_cap_ms(self, fault_backoff_cap_ms: u64) -> Self {
        Self {
            fault_backoff_cap_ms,
            ..self
        }
    }

    /// Returns the policy with `park_reprobe_ms` replaced.
    #[must_use]
    pub const fn with_park_reprobe_ms(self, park_reprobe_ms: Option<u64>) -> Self {
        Self {
            park_reprobe_ms,
            ..self
        }
    }

    fn normalized(self) -> Self {
        let fault_backoff_initial_ms = self.fault_backoff_initial_ms.max(1);
        Self {
            escalate_after_faults: self.escalate_after_faults.max(1),
            max_rescan_attempts: self.max_rescan_attempts.max(1),
            fault_backoff_initial_ms,
            fault_backoff_cap_ms: self.fault_backoff_cap_ms.max(fault_backoff_initial_ms),
            park_reprobe_ms: self.park_reprobe_ms.map(|hold_ms| hold_ms.max(1)),
        }
    }
}

impl Default for SyncRecoveryPolicy {
    fn default() -> Self {
        Self {
            escalate_after_faults: 3,
            max_rescan_attempts: 2,
            fault_backoff_initial_ms: 1_000,
            fault_backoff_cap_ms: 60_000,
            park_reprobe_ms: Some(900_000),
        }
    }
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
    /// Self-healing policy for the driver's repair ladder.
    pub recovery: SyncRecoveryPolicy,
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

    /// Returns options with `recovery` replaced.
    #[must_use]
    pub const fn with_recovery_policy(self, recovery: SyncRecoveryPolicy) -> Self {
        Self { recovery, ..self }
    }

    fn normalized(self) -> Self {
        Self {
            poll_interval_ms: self.poll_interval_ms.max(1),
            max_sync_iterations_per_wake_count: self.max_sync_iterations_per_wake_count.max(1),
            sync_timeout_seconds: self.sync_timeout_seconds.max(1),
            recovery: self.recovery.normalized(),
        }
    }
}

impl Default for SyncDriverOptions {
    fn default() -> Self {
        Self {
            poll_interval_ms: 5_000,
            max_sync_iterations_per_wake_count: 1_000,
            sync_timeout_seconds: 120,
            recovery: SyncRecoveryPolicy::default(),
        }
    }
}

/// Repair rung the sync driver applies before its next sync attempt.
///
/// Ordered by severity; the ladder only ever escalates from a lower rung to a higher one.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SyncRepair {
    /// Transient fault; the same state may succeed on the next attempt.
    Retry,
    /// The wallet's view diverged from the chain; truncate below the divergence.
    Rewind,
    /// Derived state is untrustworthy; rebuild it from the seed and the account birthday.
    RescanFromBirthday,
    /// No software action cures this; hold and keep republishing the reason.
    Park,
}

impl SyncRepair {
    /// Stable `snake_case` label for logs and metrics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Rewind => "rewind",
            Self::RescanFromBirthday => "rescan_from_birthday",
            Self::Park => "park",
        }
    }
}

/// Cloneable fault record carried by [`SyncSnapshot`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncFault {
    /// Fault description safe for logs and status pages.
    pub reason: String,
    /// Repair rung the driver applies (or holds at) for this fault.
    pub repair: SyncRepair,
    /// Unix milliseconds when the fault was observed.
    pub occurred_at_ms: u64,
    /// Consecutive ladder faults up to and including this one. `0` when the fault did not
    /// enter the ladder (chain-event stream interruptions; polling keeps sync healthy).
    pub consecutive_faults: u32,
}

/// Lifecycle phase of a running [`SyncDriver`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SyncDriverPhase {
    /// The driver task has been created and is opening its chain-event stream.
    Starting,
    /// The driver is running one or more [`Wallet::sync`] iterations.
    Syncing,
    /// Healthy idle: the last sync completed and the driver is waiting for a chain event or
    /// the next polling wakeup.
    Waiting,
    /// Degraded and self-healing: the driver observed a fault and applies `repair` before
    /// the next sync attempt.
    Recovering {
        /// Repair rung the driver applies before the next attempt.
        repair: SyncRepair,
        /// 1-based attempt number at the current rung.
        attempt: u32,
        /// Unix milliseconds when the next attempt is due.
        next_attempt_at_ms: u64,
    },
    /// Dead end: no software action cures the recorded fault. The driver holds, keeps
    /// republishing its reason, and re-arms the ladder at `reprobe_at_ms` when set.
    Parked {
        /// Unix milliseconds when the driver parked.
        since_ms: u64,
        /// Unix milliseconds when the driver re-arms the ladder, if reprobing is enabled.
        reprobe_at_ms: Option<u64>,
    },
    /// The driver is closing after the caller requested shutdown.
    Closing,
    /// The driver closed cleanly.
    Closed,
}

/// Current observable state of a [`SyncDriver`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncSnapshot {
    /// Network this driver is bound to.
    pub network: Network,
    /// Driver lifecycle phase.
    pub phase: SyncDriverPhase,
    /// Wallet scan status derived from persisted progress.
    pub sync_status: SyncStatus,
    /// Highest block height the wallet has scanned, if any.
    pub scanned_height: Option<BlockHeight>,
    /// Highest source-visible block recorded by the most recent sync, if any.
    pub visible_tip_height: Option<BlockHeight>,
    /// Settled finality height recorded by the most recent sync, if any.
    pub settled_tip_height: Option<BlockHeight>,
    /// Number of blocks between `scanned_height` and `visible_tip_height`, if known.
    pub lag_blocks: Option<u32>,
    /// Most recent completed [`Wallet::sync`] run summary.
    pub last_outcome: Option<SyncOutcome>,
    /// Most recent fault; `None` while healthy.
    pub last_fault: Option<SyncFault>,
    /// Unix milliseconds when this snapshot was published.
    pub published_at_ms: u64,
}

impl SyncSnapshot {
    fn starting(network: Network) -> Self {
        Self {
            network,
            phase: SyncDriverPhase::Starting,
            sync_status: SyncStatus::NotStarted,
            scanned_height: None,
            visible_tip_height: None,
            settled_tip_height: None,
            lag_blocks: None,
            last_outcome: None,
            last_fault: None,
            published_at_ms: current_unix_ms(),
        }
    }

    fn from_wallet_status(
        phase: SyncDriverPhase,
        wallet_status: &WalletStatus,
        last_outcome: Option<SyncOutcome>,
        last_fault: Option<SyncFault>,
    ) -> Self {
        Self {
            network: wallet_status.network,
            phase,
            sync_status: wallet_status.sync_status,
            scanned_height: wallet_status.scanned_height,
            visible_tip_height: wallet_status.visible_tip_height,
            settled_tip_height: wallet_status.settled_tip_height,
            lag_blocks: wallet_status.lag_blocks,
            last_outcome,
            last_fault,
            published_at_ms: current_unix_ms(),
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
///
/// The driver task is infallible while its handle is alive. Faults engage the escalating
/// repair ladder ([`SyncRepair`]) instead of killing the task; the task exits only through
/// [`SyncHandle::close`].
pub struct SyncDriver {
    wallet: Wallet,
    chain: Arc<dyn ChainSource>,
    options: SyncDriverOptions,
}

impl SyncDriver {
    /// Constructs a driver for `wallet` and `chain`.
    ///
    /// Fails closed on network mismatch. `requires_operator` on mismatch.
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
    join: JoinHandle<()>,
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
    ///
    /// The driver task never fails on its own, so the only close-time error is
    /// [`WalletError::SyncDriverFailed`] from a panic inside the driver task.
    pub async fn close(mut self) -> Result<(), WalletError> {
        if let Some(close_tx) = self.close_tx.take() {
            let _ = close_tx.send(());
        }
        match self.join.await {
            Ok(()) => Ok(()),
            Err(join_error) if join_error.is_panic() => Err(WalletError::SyncDriverFailed {
                reason: join_error.to_string(),
            }),
            Err(_cancelled) => Ok(()),
        }
    }
}

struct DriverContext<'a> {
    wallet: &'a Wallet,
    chain: &'a dyn ChainSource,
    options: SyncDriverOptions,
    status_tx: &'a watch::Sender<SyncSnapshot>,
}

#[derive(Default)]
struct DriverState {
    last_outcome: Option<SyncOutcome>,
    last_fault: Option<SyncFault>,
    recovery: Option<RecoveryState>,
    cursor_recovery_pending: bool,
    stream_consecutive_faults: u32,
    stream_next_attempt_at_ms: u64,
    stream_reprobe: StreamReprobe,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum StreamReprobe {
    #[default]
    Inactive,
    ParkedWithoutDeadline,
    At(u64),
}

impl StreamReprobe {
    const fn deadline(self) -> Option<u64> {
        match self {
            Self::At(deadline) => Some(deadline),
            Self::Inactive | Self::ParkedWithoutDeadline => None,
        }
    }
}

impl DriverState {
    fn parked(&self) -> Option<ParkedAt> {
        self.recovery.as_ref().and_then(|recovery| recovery.parked)
    }

    /// Settles the active recovery after a completed or excused sync iteration.
    ///
    /// Returns the recovery when the scan passed the fault boundary (a genuine repair
    /// success). When the boundary has not been passed, the ladder position is retained
    /// dormant so that a recurring fault resumes the ladder where it left off instead of
    /// restarting it at the first rung: a completed re-scan of already-known-good blocks
    /// below the conflict proves nothing about the conflict itself (issue #5).
    fn settle_recovery(&mut self, scanned_to: Option<BlockHeight>) -> Option<RecoveryState> {
        let survives = self.recovery.as_ref().is_some_and(|recovery| {
            recovery.fault_height.is_some_and(|fault_height| {
                scanned_to.is_none_or(|scanned| scanned <= fault_height)
            })
        });
        if survives {
            if let Some(recovery) = self.recovery.as_mut() {
                recovery.dormant = true;
            }
            None
        } else {
            self.recovery.take()
        }
    }
}

struct RecoveryState {
    rung: SyncRepair,
    max_classified: SyncRepair,
    attempts_at_rung: u32,
    rewind_depth_index: usize,
    consecutive_faults: u32,
    backoff_ms: u64,
    degraded_since_ms: u64,
    parked: Option<ParkedAt>,
    /// Highest wallet scanned height observed at fault time. Recovery is complete only
    /// when a sync finishes strictly above this height; anything at or below it re-covers
    /// known-good ground and must not clear the ladder.
    fault_height: Option<BlockHeight>,
    /// A dormant recovery no longer applies repairs or backoff; it survives completed
    /// syncs below `fault_height` purely as ladder memory and is woken by the next fault.
    dormant: bool,
}

impl RecoveryState {
    const fn entering(rung: SyncRepair, now_ms: u64) -> Self {
        Self {
            rung,
            max_classified: rung,
            attempts_at_rung: 0,
            rewind_depth_index: 0,
            consecutive_faults: 0,
            backoff_ms: 0,
            degraded_since_ms: now_ms,
            parked: None,
            fault_height: None,
            dormant: false,
        }
    }

    /// Folds one classified fault into the ladder, escalating the rung when the current rung
    /// has exhausted its attempts. Returns the rung transition when one occurred.
    fn fold_fault(
        &mut self,
        classified: SyncRepair,
        policy: SyncRecoveryPolicy,
    ) -> Option<(SyncRepair, SyncRepair)> {
        self.max_classified = self.max_classified.max(classified);
        let escalation = if classified > self.rung {
            self.rung = classified;
            self.attempts_at_rung = 0;
            if classified == SyncRepair::Rewind {
                self.rewind_depth_index = 0;
            }
            None
        } else if self.attempts_at_rung >= escalation_threshold(self.rung, policy) {
            let from_repair = self.rung;
            escalate(self);
            Some((from_repair, self.rung))
        } else {
            None
        };
        self.consecutive_faults = self.consecutive_faults.saturating_add(1);
        self.backoff_ms = backoff_for(policy, self.consecutive_faults);
        escalation
    }
}

#[derive(Clone, Copy)]
struct ParkedAt {
    since_ms: u64,
    reprobe_at_ms: Option<u64>,
}

enum SyncRunAttempt {
    Completed(SyncOutcome),
    Faulted { reason: String, repair: SyncRepair },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncWakeupExit {
    Reconciled,
    Pending,
    CloseRequested,
}

enum DriverTransition<'a> {
    DriverStarted,
    Fault {
        fault: &'a SyncFault,
    },
    SlowProgress {
        reason: &'a str,
        blocks_advanced: u32,
    },
    RepairStarted {
        repair: SyncRepair,
        attempt: u32,
        rewind_to_height: Option<BlockHeight>,
        backoff_ms: u64,
    },
    RepairSucceeded {
        repair: SyncRepair,
        total_faults: u32,
        degraded_for_ms: u64,
    },
    RepairEscalated {
        from_repair: SyncRepair,
        to_repair: SyncRepair,
    },
    Parked {
        reason: &'a str,
        reprobe_at_ms: Option<u64>,
    },
    ParkReprobe,
    Closing,
    Closed,
}

async fn run_sync_driver(
    wallet: Wallet,
    chain: Arc<dyn ChainSource>,
    options: SyncDriverOptions,
    mut close_rx: oneshot::Receiver<()>,
    status_tx: watch::Sender<SyncSnapshot>,
) {
    let ctx = DriverContext {
        wallet: &wallet,
        chain: chain.as_ref(),
        options,
        status_tx: &status_tx,
    };
    let started = ctx.status_tx.borrow().clone();
    publish_transition(ctx.status_tx, started, &DriverTransition::DriverStarted);

    let mut poll = interval(Duration::from_millis(options.poll_interval_ms));
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut state = DriverState::default();
    let mut chain_events_start = ChainEventStreamStart::EarliestRetained;
    let mut chain_events = open_chain_events(&ctx, &mut state, &mut chain_events_start).await;
    let mut should_sync = true;

    loop {
        if should_sync && state.parked().is_none() {
            match run_sync_wakeup(&ctx, &mut close_rx, &mut state).await {
                SyncWakeupExit::CloseRequested => {
                    return close_sync_driver(&ctx, &state).await;
                }
                SyncWakeupExit::Reconciled => should_sync = false,
                SyncWakeupExit::Pending => should_sync = true,
            }
        }

        tokio::select! {
            _ = &mut close_rx => {
                return close_sync_driver(&ctx, &state).await;
            }
            _ = poll.tick() => {
                should_sync = handle_poll_tick(
                    &ctx,
                    &mut state,
                    &mut chain_events,
                    &mut chain_events_start,
                )
                .await;
            }
            chain_event = next_chain_event_envelope(&mut chain_events) => {
                match chain_event {
                    Some(Ok(envelope)) => {
                        chain_events_start = ChainEventStreamStart::AfterCursor(envelope.cursor);
                        should_sync = state.parked().is_none();
                    }
                    Some(Err(err)) => {
                        chain_events = None;
                        if is_expired_cursor(&err) {
                            chain_events_start = ChainEventStreamStart::EarliestRetained;
                            should_sync = state.parked().is_none();
                        } else if state.parked().is_none() {
                            record_stream_fault(&ctx, &mut state, &err).await;
                        }
                    }
                    None => {
                        chain_events = None;
                    }
                }
            }
        }
    }
}

/// Handles one polling wakeup; returns whether the driver should sync.
///
/// While parked this republishes the current snapshot (refreshed `published_at_ms`) so
/// observers keep receiving the parked reason, and re-arms the full ladder once the reprobe
/// deadline passes.
async fn handle_poll_tick(
    ctx: &DriverContext<'_>,
    state: &mut DriverState,
    chain_events: &mut Option<ChainEventEnvelopeStream>,
    start: &mut ChainEventStreamStart,
) -> bool {
    if let Some(parked) = state.parked() {
        if parked
            .reprobe_at_ms
            .is_some_and(|reprobe_at_ms| current_unix_ms() >= reprobe_at_ms)
        {
            state.recovery = None;
            let snapshot = build_snapshot(ctx, SyncDriverPhase::Waiting, state).await;
            publish_transition(ctx.status_tx, snapshot, &DriverTransition::ParkReprobe);
            return true;
        }
        let refreshed = ctx.status_tx.borrow().clone();
        publish_snapshot(ctx.status_tx, refreshed);
        return false;
    }
    if chain_events.is_none() {
        let now = current_unix_ms();
        match state.stream_reprobe {
            StreamReprobe::ParkedWithoutDeadline => return false,
            StreamReprobe::At(deadline) if now < deadline => return false,
            StreamReprobe::At(_) => {
                state.stream_reprobe = StreamReprobe::Inactive;
                state.stream_consecutive_faults = 0;
                state.stream_next_attempt_at_ms = 0;
            }
            StreamReprobe::Inactive => {}
        }
        if now >= state.stream_next_attempt_at_ms {
            *chain_events = open_chain_events(ctx, state, start).await;
        }
    }
    std::mem::take(&mut state.cursor_recovery_pending)
}

async fn close_sync_driver(ctx: &DriverContext<'_>, state: &DriverState) {
    let closing = build_snapshot(ctx, SyncDriverPhase::Closing, state).await;
    publish_transition(ctx.status_tx, closing, &DriverTransition::Closing);
    let closed = build_snapshot(ctx, SyncDriverPhase::Closed, state).await;
    publish_transition(ctx.status_tx, closed, &DriverTransition::Closed);
}

async fn run_sync_wakeup(
    ctx: &DriverContext<'_>,
    close_rx: &mut oneshot::Receiver<()>,
    state: &mut DriverState,
) -> SyncWakeupExit {
    for _ in 0..ctx.options.max_sync_iterations_per_wake_count {
        if let Some(recovery) = &state.recovery
            && !recovery.dormant
        {
            if recovery.rung == SyncRepair::Park {
                enter_park(ctx, state).await;
                return SyncWakeupExit::Pending;
            }
            let backoff_ms = recovery.backoff_ms;
            tokio::select! {
                biased;
                _ = &mut *close_rx => return SyncWakeupExit::CloseRequested,
                () = sleep(Duration::from_millis(backoff_ms)) => {}
            }
            if let Err(repair_error) = apply_repair(ctx, state).await {
                let repair = repair_for(&repair_error);
                record_fault(ctx, state, repair_error.to_string(), repair, None).await;
                continue;
            }
        }
        let scanned_before = tokio::select! {
            biased;
            _ = &mut *close_rx => return SyncWakeupExit::CloseRequested,
            snapshot = build_snapshot(ctx, SyncDriverPhase::Syncing, state) => {
                let scanned_before = snapshot.scanned_height;
                publish_snapshot(ctx.status_tx, snapshot);
                scanned_before
            }
        };
        let attempt = tokio::select! {
            biased;
            _ = &mut *close_rx => return SyncWakeupExit::CloseRequested,
            attempt = run_one_sync(ctx.wallet, ctx.chain, ctx.options) => attempt,
        };
        match attempt {
            SyncRunAttempt::Completed(outcome) => {
                if !complete_sync(ctx, state, outcome).await {
                    return SyncWakeupExit::Reconciled;
                }
            }
            SyncRunAttempt::Faulted { reason, repair } => {
                let scanned_after = ctx
                    .wallet
                    .status_snapshot()
                    .await
                    .ok()
                    .and_then(|status| status.scanned_height);
                let blocks_advanced = height_delta(scanned_before, scanned_after);
                if is_slow_progress(repair, blocks_advanced) {
                    note_slow_progress(ctx, state, reason, blocks_advanced, scanned_after).await;
                } else {
                    record_fault(ctx, state, reason, repair, scanned_after).await;
                }
            }
        }
    }
    SyncWakeupExit::Pending
}

/// Publishes the outcome of a completed sync run.
///
/// Returns whether the wakeup should run another iteration. Announces a repair success
/// only when the scan passed the recovery's fault boundary; a completed sync at or below
/// it retains the ladder position dormant.
async fn complete_sync(
    ctx: &DriverContext<'_>,
    state: &mut DriverState,
    outcome: SyncOutcome,
) -> bool {
    let recovered = state.settle_recovery(Some(outcome.scanned_to_height));
    state.last_outcome = Some(outcome);
    state.last_fault = None;
    let should_continue = should_continue_syncing(outcome);
    let phase = if should_continue {
        SyncDriverPhase::Syncing
    } else {
        SyncDriverPhase::Waiting
    };
    let snapshot = build_snapshot(ctx, phase, state).await;
    if let Some(recovery) = recovered {
        let degraded_for_ms = current_unix_ms().saturating_sub(recovery.degraded_since_ms);
        publish_transition(
            ctx.status_tx,
            snapshot,
            &DriverTransition::RepairSucceeded {
                repair: recovery.rung,
                total_faults: recovery.consecutive_faults,
                degraded_for_ms,
            },
        );
    } else {
        publish_snapshot(ctx.status_tx, snapshot);
    }
    should_continue
}

/// Handles an environment fault whose iteration still advanced the wallet's scanned height.
///
/// Presents the driver as healthy again without applying a repair or backoff. Ladder
/// memory survives dormant unless the advance passed the recovery's fault boundary.
async fn note_slow_progress(
    ctx: &DriverContext<'_>,
    state: &mut DriverState,
    reason: String,
    blocks_advanced: u32,
    scanned_after: Option<BlockHeight>,
) {
    state.settle_recovery(scanned_after);
    state.last_fault = None;
    let snapshot = build_snapshot(ctx, SyncDriverPhase::Syncing, state).await;
    publish_transition(
        ctx.status_tx,
        snapshot,
        &DriverTransition::SlowProgress {
            reason: &reason,
            blocks_advanced,
        },
    );
}

/// Blocks the wallet's scanned height advanced between two reads, treating an absent height
/// as zero.
fn height_delta(before: Option<BlockHeight>, after: Option<BlockHeight>) -> u32 {
    let before = before.map_or(0, BlockHeight::as_u32);
    let after = after.map_or(0, BlockHeight::as_u32);
    after.saturating_sub(before)
}

/// Whether a faulted iteration counts as slow progress instead of a ladder strike.
///
/// Only environment faults (classified [`SyncRepair::Retry`]) are excused by forward
/// progress. A state fault after a committed chunk still proves divergence; skipping its
/// repair would scan the next chunk on top of the corrupt state.
const fn is_slow_progress(repair: SyncRepair, blocks_advanced: u32) -> bool {
    matches!(repair, SyncRepair::Retry) && blocks_advanced > 0
}

/// Folds a fault into the recovery ladder and publishes the resulting transition.
///
/// The entry rung is the maximum of the current rung and the fault's classification; the
/// ladder never de-escalates within one degraded episode. Once the current rung has been
/// applied [`SyncRecoveryPolicy::escalate_after_faults`] times without a completed sync
/// (rebuilds use [`SyncRecoveryPolicy::max_rescan_attempts`]), the next fault escalates one
/// rung.
async fn record_fault(
    ctx: &DriverContext<'_>,
    state: &mut DriverState,
    reason: String,
    classified: SyncRepair,
    fault_height: Option<BlockHeight>,
) {
    let now = current_unix_ms();
    let policy = ctx.options.recovery;
    let recovery = state
        .recovery
        .get_or_insert_with(|| RecoveryState::entering(classified, now));
    recovery.dormant = false;
    if let Some(height) = fault_height {
        // Rewinds lower the scanned height, so a fault observed after one must not lower
        // the recovery bar below the original conflict.
        recovery.fault_height = Some(
            recovery
                .fault_height
                .map_or(height, |prior| prior.max(height)),
        );
    }
    let escalation = recovery.fold_fault(classified, policy);
    let fault = SyncFault {
        reason,
        repair: recovery.rung,
        occurred_at_ms: now,
        consecutive_faults: recovery.consecutive_faults,
    };
    let phase = SyncDriverPhase::Recovering {
        repair: recovery.rung,
        attempt: recovery.attempts_at_rung.saturating_add(1),
        next_attempt_at_ms: now.saturating_add(recovery.backoff_ms),
    };
    state.last_fault = Some(fault.clone());
    let snapshot = build_snapshot(ctx, phase, state).await;
    publish_transition(
        ctx.status_tx,
        snapshot,
        &DriverTransition::Fault { fault: &fault },
    );
    if let Some((from_repair, to_repair)) = escalation {
        let snapshot = ctx.status_tx.borrow().clone();
        publish_transition(
            ctx.status_tx,
            snapshot,
            &DriverTransition::RepairEscalated {
                from_repair,
                to_repair,
            },
        );
    }
}

fn escalate(recovery: &mut RecoveryState) {
    match recovery.rung {
        SyncRepair::Retry => {
            // A slow or unreachable upstream is not cured by rewinding or rebuilding; only a
            // classified state fault earns a state repair.
            if recovery.max_classified >= SyncRepair::Rewind {
                recovery.rung = SyncRepair::Rewind;
                recovery.rewind_depth_index = 0;
            } else {
                recovery.rung = SyncRepair::Park;
            }
        }
        SyncRepair::Rewind => {
            if recovery.rewind_depth_index + 1 < REWIND_LADDER_BLOCKS.len() {
                recovery.rewind_depth_index += 1;
            } else {
                recovery.rung = SyncRepair::RescanFromBirthday;
            }
        }
        SyncRepair::RescanFromBirthday => recovery.rung = SyncRepair::Park,
        SyncRepair::Park => {}
    }
    recovery.attempts_at_rung = 0;
}

const fn escalation_threshold(rung: SyncRepair, policy: SyncRecoveryPolicy) -> u32 {
    match rung {
        SyncRepair::RescanFromBirthday => policy.max_rescan_attempts,
        SyncRepair::Retry | SyncRepair::Rewind | SyncRepair::Park => policy.escalate_after_faults,
    }
}

const fn backoff_for(policy: SyncRecoveryPolicy, consecutive_faults: u32) -> u64 {
    let exponent = consecutive_faults.saturating_sub(1);
    let exponent = if exponent > 31 { 31 } else { exponent };
    let scaled = policy
        .fault_backoff_initial_ms
        .saturating_mul(1_u64 << exponent);
    if scaled > policy.fault_backoff_cap_ms {
        policy.fault_backoff_cap_ms
    } else {
        scaled
    }
}

/// Classifies a fault onto the repair ladder.
///
/// The named arms pin the cures the posture cannot express: commitment-tree conflicts,
/// scan-time reorg divergences, and proven tree-root divergence rewind below the
/// divergence. Every other error derives its repair from [`WalletError::posture`]:
/// transient faults retry, operator dead ends park (the literal
/// [`FailurePosture::RequiresOperator`] definition), and the rest rewind: the ladder
/// escalates to a rebuild when rewinding does not cure, which is the self-healing default
/// for unknown corruption.
fn repair_for(error: &WalletError) -> SyncRepair {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "the named arms pin the cures that posture cannot express; every other \
                  error derives its repair from its posture, keeping the classification \
                  total"
    )]
    match error {
        WalletError::Storage(
            StorageError::CommitmentTreeConflict { .. }
            | StorageError::ChainReorgDetected { .. }
            // A dropped reply means the storage work panicked and unwound; a panic during
            // scanning is state-shaped, and repeating the same call proves nothing. Rewind,
            // and let the ladder escalate to a rebuild if the panic recurs.
            | StorageError::BlockingTaskFailed { .. },
        )
        | WalletError::TreeRootsDiverged { .. } => SyncRepair::Rewind,
        other => match other.posture() {
            FailurePosture::Retryable => SyncRepair::Retry,
            FailurePosture::RequiresOperator => SyncRepair::Park,
            _ => SyncRepair::Rewind,
        },
    }
}

/// Applies the current repair rung before the next sync attempt.
///
/// A failed repair is itself a fault: the caller records it and the ladder escalates
/// naturally.
async fn apply_repair(ctx: &DriverContext<'_>, state: &mut DriverState) -> Result<(), WalletError> {
    let Some(recovery) = state.recovery.as_mut() else {
        return Ok(());
    };
    recovery.attempts_at_rung = recovery.attempts_at_rung.saturating_add(1);
    let repair = recovery.rung;
    let attempt = recovery.attempts_at_rung;
    let backoff_ms = recovery.backoff_ms;
    let rewind_depth = REWIND_LADDER_BLOCKS[recovery.rewind_depth_index];

    let phase = SyncDriverPhase::Recovering {
        repair,
        attempt,
        next_attempt_at_ms: current_unix_ms(),
    };
    let (snapshot, rewind_to_height) = if repair == SyncRepair::Rewind {
        let wallet_status = ctx.wallet.status_snapshot().await?;
        let rewind_to_height = wallet_status
            .scanned_height
            .map(|scanned| BlockHeight::from(scanned.as_u32().saturating_sub(rewind_depth)));
        let snapshot = SyncSnapshot::from_wallet_status(
            phase,
            &wallet_status,
            state.last_outcome,
            state.last_fault.clone(),
        );
        (snapshot, rewind_to_height)
    } else {
        (build_snapshot(ctx, phase, state).await, None)
    };
    publish_transition(
        ctx.status_tx,
        snapshot,
        &DriverTransition::RepairStarted {
            repair,
            attempt,
            rewind_to_height,
            backoff_ms,
        },
    );
    match repair {
        SyncRepair::Retry | SyncRepair::Park => Ok(()),
        SyncRepair::Rewind => {
            if let Some(rewind_to) = rewind_to_height {
                ctx.wallet.rewind_to_height(ctx.chain, rewind_to).await?;
            }
            Ok(())
        }
        SyncRepair::RescanFromBirthday => ctx.wallet.reset_to_birthday(ctx.chain).await,
    }
}

async fn enter_park(ctx: &DriverContext<'_>, state: &mut DriverState) {
    let now = current_unix_ms();
    let reprobe_at_ms = ctx
        .options
        .recovery
        .park_reprobe_ms
        .map(|hold_ms| now.saturating_add(hold_ms));
    let parked = ParkedAt {
        since_ms: now,
        reprobe_at_ms,
    };
    if let Some(recovery) = state.recovery.as_mut() {
        recovery.parked = Some(parked);
    }
    let reason = state
        .last_fault
        .as_ref()
        .map_or_else(String::new, |fault| fault.reason.clone());
    let snapshot = build_snapshot(
        ctx,
        SyncDriverPhase::Parked {
            since_ms: parked.since_ms,
            reprobe_at_ms,
        },
        state,
    )
    .await;
    publish_transition(
        ctx.status_tx,
        snapshot,
        &DriverTransition::Parked {
            reason: &reason,
            reprobe_at_ms,
        },
    );
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
        Ok(Err(error)) => SyncRunAttempt::Faulted {
            reason: error.to_string(),
            repair: repair_for(&error),
        },
        Err(_elapsed) => SyncRunAttempt::Faulted {
            reason: format!("sync exceeded {} seconds", options.sync_timeout_seconds),
            repair: SyncRepair::Retry,
        },
    }
}

/// Whether the driver should run another sync iteration in this wakeup.
///
/// A cycle that scanned a chunk (`block_count > 0`) leaves more scan-queue work; a
/// caught-up cycle reports none and stops the loop until the next chain event or poll.
const fn should_continue_syncing(outcome: SyncOutcome) -> bool {
    outcome.block_count > 0
}

/// Builds a snapshot from the live wallet status, falling back to the previously published
/// snapshot when the status read fails (the driver must keep publishing regardless).
async fn build_snapshot(
    ctx: &DriverContext<'_>,
    phase: SyncDriverPhase,
    state: &DriverState,
) -> SyncSnapshot {
    match ctx.wallet.status_snapshot().await {
        Ok(wallet_status) => SyncSnapshot::from_wallet_status(
            phase,
            &wallet_status,
            state.last_outcome,
            state.last_fault.clone(),
        ),
        Err(_status_unavailable) => {
            let mut snapshot = ctx.status_tx.borrow().clone();
            snapshot.phase = phase;
            snapshot.last_outcome = state.last_outcome;
            snapshot.last_fault.clone_from(&state.last_fault);
            snapshot
        }
    }
}

/// Records a chain-event stream interruption and applies bounded reconnect backoff.
async fn record_stream_fault(
    ctx: &DriverContext<'_>,
    state: &mut DriverState,
    error: &ChainSourceError,
) {
    let now = current_unix_ms();
    state.stream_consecutive_faults = state.stream_consecutive_faults.saturating_add(1);
    let backoff_ms = backoff_for(ctx.options.recovery, state.stream_consecutive_faults);
    state.stream_next_attempt_at_ms = now.saturating_add(backoff_ms);
    let should_park = state.stream_consecutive_faults >= ctx.options.recovery.escalate_after_faults;
    if should_park {
        state.stream_reprobe = ctx
            .options
            .recovery
            .park_reprobe_ms
            .map_or(StreamReprobe::ParkedWithoutDeadline, |hold_ms| {
                StreamReprobe::At(now.saturating_add(hold_ms))
            });
    }
    let fault = SyncFault {
        reason: error.to_string(),
        repair: if should_park {
            SyncRepair::Park
        } else {
            SyncRepair::Retry
        },
        occurred_at_ms: now,
        consecutive_faults: state.stream_consecutive_faults,
    };
    state.last_fault = Some(fault);
    let phase = if should_park {
        SyncDriverPhase::Parked {
            since_ms: now,
            reprobe_at_ms: state.stream_reprobe.deadline(),
        }
    } else {
        SyncDriverPhase::Recovering {
            repair: SyncRepair::Retry,
            attempt: state.stream_consecutive_faults,
            next_attempt_at_ms: state.stream_next_attempt_at_ms,
        }
    };
    let snapshot = build_snapshot(ctx, phase, state).await;
    publish_snapshot(ctx.status_tx, snapshot);
}

/// Single choke point for lifecycle transitions: emits the tracing event, then publishes
/// the snapshot.
fn publish_transition(
    status_tx: &watch::Sender<SyncSnapshot>,
    snapshot: SyncSnapshot,
    transition: &DriverTransition<'_>,
) {
    emit_transition_event(&snapshot, transition);
    publish_snapshot(status_tx, snapshot);
}

#[allow(
    clippy::too_many_lines,
    reason = "one flat arm per lifecycle transition; splitting would scatter the sync event vocabulary across helpers"
)]
fn emit_transition_event(snapshot: &SyncSnapshot, transition: &DriverTransition<'_>) {
    match transition {
        DriverTransition::DriverStarted => tracing::info!(
            target: "zally::sync",
            event = "wallet_sync_driver_started",
            network = ?snapshot.network,
            "sync driver task started"
        ),
        DriverTransition::Fault { fault } => tracing::warn!(
            target: "zally::sync",
            event = "wallet_sync_fault",
            reason = %fault.reason,
            repair = fault.repair.label(),
            consecutive_faults = fault.consecutive_faults,
            scanned_height = snapshot.scanned_height.map(BlockHeight::as_u32),
            "sync fault; repair ladder engaged"
        ),
        DriverTransition::SlowProgress {
            reason,
            blocks_advanced,
        } => tracing::warn!(
            target: "zally::sync",
            event = "wallet_sync_slow_progress",
            reason = %reason,
            blocks_advanced,
            scanned_height = snapshot.scanned_height.map(BlockHeight::as_u32),
            "sync faulted mid-chunk but advanced; ladder reset"
        ),
        DriverTransition::RepairStarted {
            repair,
            attempt,
            rewind_to_height,
            backoff_ms,
        } => tracing::warn!(
            target: "zally::sync",
            event = "wallet_sync_repair_started",
            repair = repair.label(),
            attempt,
            rewind_to_height = rewind_to_height.map(BlockHeight::as_u32),
            backoff_ms,
            "applying repair before the next sync attempt"
        ),
        DriverTransition::RepairSucceeded {
            repair,
            total_faults,
            degraded_for_ms,
        } => tracing::info!(
            target: "zally::sync",
            event = "wallet_sync_repair_succeeded",
            repair = repair.label(),
            total_faults,
            degraded_for_ms,
            "sync completed after repair; driver healthy"
        ),
        DriverTransition::RepairEscalated {
            from_repair,
            to_repair,
        } => tracing::error!(
            target: "zally::sync",
            event = "wallet_sync_repair_escalated",
            from_repair = from_repair.label(),
            to_repair = to_repair.label(),
            "repair did not cure the fault; escalating to a deeper rung"
        ),
        DriverTransition::Parked {
            reason,
            reprobe_at_ms,
        } => tracing::error!(
            target: "zally::sync",
            event = "wallet_sync_parked",
            reason = %reason,
            reprobe_at_ms,
            "no software repair cures this fault; driver parked"
        ),
        DriverTransition::ParkReprobe => tracing::info!(
            target: "zally::sync",
            event = "wallet_sync_park_reprobe",
            "park hold elapsed; repair ladder re-armed"
        ),
        DriverTransition::Closing => tracing::info!(
            target: "zally::sync",
            event = "wallet_sync_driver_closing",
            "sync driver closing on request"
        ),
        DriverTransition::Closed => tracing::info!(
            target: "zally::sync",
            event = "wallet_sync_driver_closed",
            "sync driver closed"
        ),
    }
}

fn publish_snapshot(status_tx: &watch::Sender<SyncSnapshot>, mut snapshot: SyncSnapshot) {
    snapshot.published_at_ms = current_unix_ms();
    let _ = status_tx.send(snapshot);
}

async fn open_chain_events(
    ctx: &DriverContext<'_>,
    state: &mut DriverState,
    start: &mut ChainEventStreamStart,
) -> Option<ChainEventEnvelopeStream> {
    match ctx.chain.chain_event_envelopes(start.clone()).await {
        Ok(stream) => {
            let recovered_stream = state.stream_consecutive_faults > 0;
            state.stream_consecutive_faults = 0;
            state.stream_next_attempt_at_ms = 0;
            state.stream_reprobe = StreamReprobe::Inactive;
            if recovered_stream {
                state.last_fault = None;
                let snapshot = build_snapshot(ctx, SyncDriverPhase::Waiting, state).await;
                publish_snapshot(ctx.status_tx, snapshot);
            }
            Some(stream)
        }
        Err(err) if is_expired_cursor(&err) => {
            *start = ChainEventStreamStart::EarliestRetained;
            state.cursor_recovery_pending = true;
            None
        }
        Err(err) => {
            record_stream_fault(ctx, state, &err).await;
            None
        }
    }
}

fn is_expired_cursor(error: &ChainSourceError) -> bool {
    matches!(
        error,
        ChainSourceError::ChainEventCursorExpired {
            recovery: ChainEventCursorRecovery::EarliestRetained,
        }
    )
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
    /// Advances the wallet by one bounded scan step toward the current epoch's visible tip.
    ///
    /// Each call primes commitment-tree subtree roots, calls `update_chain_tip` with the live
    /// tip, then scans the highest-priority range `suggest_scan_ranges` returns (chunked to
    /// `MAX_BLOCKS_PER_SYNC`). The `SyncDriver` loops until the scan queue drains. Subtree
    /// roots let the wallet witness a note from its subtree root without scanning every block,
    /// so spendability does not require a full linear scan; transaction expiry heights are
    /// computed against the live tip. Reorg safety comes from the spend-time confirmation
    /// depth (ZIP 315); scan-time divergences (`ChainReorgDetected`,
    /// `CommitmentTreeConflict`, [`WalletError::TreeRootsDiverged`]) surface as errors that
    /// the [`SyncDriver`] repairs by rewinding or rebuilding derived state.
    ///
    /// Fails closed on network mismatch. Emits `ScanProgress` events at the start and end of
    /// the run; per-block events are emitted by the storage scanner.
    ///
    /// `requires_operator` on network mismatch. `retryable` on transient chain-source
    /// failures.
    pub async fn sync(&self, chain: &dyn ChainSource) -> Result<SyncOutcome, WalletError> {
        let outcome = with_breaker_and_retry(
            &self.inner.circuit_breaker,
            self.retry_policy(),
            "sync.attempt",
            || self.sync_inner(chain),
            std::convert::identity,
        )
        .await?;
        self.retire_expired_pending_broadcasts().await?;
        Ok(outcome)
    }

    async fn retire_expired_pending_broadcasts(&self) -> Result<(), WalletError> {
        let before_at_ms =
            current_unix_ms().saturating_sub(self.inner.options.pending_broadcast_window_ms);
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
        let chain_epoch = chain.current_epoch().await?;
        let visible_tip = chain_epoch.visible_tip().height;
        let settled_tip = chain_epoch.settled_tip().height;
        self.inner.storage.update_chain_tip(visible_tip).await?;
        self.inner
            .storage
            .record_chain_tips(visible_tip, settled_tip)
            .await?;

        let Some((scan_start, scan_end, priority)) = self
            .plan_scan_range(chain, chain_epoch, visible_tip)
            .await?
        else {
            let transparent_utxo_count = self.sync_transparent_utxos(chain, chain_epoch).await?;
            return Ok(self.emit_caught_up(visible_tip, transparent_utxo_count));
        };
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: scan_start,
            target_height: visible_tip,
        });
        tracing::info!(
            target: "zally::sync",
            event = "wallet_sync_cycle",
            scanned_from = scan_start.as_u32(),
            scan_end = scan_end.as_u32(),
            settled_tip = settled_tip.as_u32(),
            visible_tip = visible_tip.as_u32(),
            priority,
            "sync cycle: scanning a suggested range chunk"
        );

        let from_state = fetch_prior_chain_state(chain, chain_epoch, scan_start).await?;
        let blocks = fetch_compact_blocks(chain, chain_epoch, scan_start, scan_end).await?;
        let block_count = u64::try_from(blocks.len()).unwrap_or(u64::MAX);
        tracing::info!(
            target: "zally::sync",
            event = "wallet_sync_fetched",
            scanned_from = scan_start.as_u32(),
            scan_end = scan_end.as_u32(),
            block_count,
            "fetched compact blocks for scan"
        );
        self.scan_and_emit(
            ScanContext {
                blocks,
                scanned_from: scan_start,
                target_height: visible_tip,
                block_count,
            },
            from_state,
            chain,
            chain_epoch,
        )
        .await
    }

    /// Resolves the next scan range at or below the visible tip.
    ///
    /// `sync_inner` updates the wallet database with the same visible tip used here. Keeping the
    /// upstream chain tip and scan frontier aligned prevents an unscanned settlement-window gap
    /// from making notes in an incomplete commitment-tree shard appear unspendable.
    async fn plan_scan_range(
        &self,
        chain: &dyn ChainSource,
        chain_epoch: zally_chain::ChainEpoch,
        visible_tip: BlockHeight,
    ) -> Result<Option<(BlockHeight, BlockHeight, &'static str)>, WalletError> {
        if let Some(range) = self.next_scan_range(visible_tip).await? {
            return Ok(Some(range));
        }
        let fully_scanned = self.inner.storage.fully_scanned_height().await?;
        if fully_scanned.is_none_or(|h| visible_tip.as_u32() > h.as_u32()) {
            self.backfill_subtree_roots(chain, chain_epoch, fully_scanned)
                .await?;
            self.next_scan_range(visible_tip).await
        } else {
            Ok(None)
        }
    }

    /// Returns the highest-priority suggested scan range that lies at or below `chain_tip`,
    /// chunked to at most [`MAX_BLOCKS_PER_SYNC`] blocks, as `(start, end_inclusive,
    /// priority_label)`. `None` when nothing at or below the visible tip remains to scan.
    ///
    /// Ranges are clamped to `visible_tip`: a suggested range can start above that height when the
    /// wallet birthday is ahead of the chain (the chain has not reached it yet), and a range
    /// can extend past the tip if the queue was planned against a higher tip; neither is
    /// fetchable, so both are skipped or trimmed.
    async fn next_scan_range(
        &self,
        visible_tip: BlockHeight,
    ) -> Result<Option<(BlockHeight, BlockHeight, &'static str)>, WalletError> {
        let tip = visible_tip.as_u32();
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

    /// Fetches and records every new subtree root for all shielded pools.
    ///
    /// Idempotent: re-recording a root the wallet already holds is a no-op, so this runs from
    /// index 0 each cycle and stops at the first short page. Subtree roots are part of the
    /// required native sync contract, so any source failure aborts the current sync attempt.
    async fn backfill_subtree_roots(
        &self,
        chain: &dyn ChainSource,
        chain_epoch: zally_chain::ChainEpoch,
        scan_floor: Option<BlockHeight>,
    ) -> Result<(), WalletError> {
        for (pool, protocol) in [
            (ShieldedPool::Sapling, zcash_protocol::ShieldedPool::Sapling),
            (ShieldedPool::Orchard, zcash_protocol::ShieldedPool::Orchard),
            (
                ShieldedPool::Ironwood,
                zcash_protocol::ShieldedPool::Ironwood,
            ),
        ] {
            let mut next_index = 0_u32;
            loop {
                let roots = chain
                    .subtree_roots(
                        chain_epoch,
                        pool,
                        SubtreeIndex(next_index),
                        SUBTREE_ROOT_PAGE,
                    )
                    .await?;
                validate_subtree_root_page(pool, next_index, SUBTREE_ROOT_PAGE, &roots)?;
                let page_len = roots.len();
                let roots = subtree_roots_completed_at_or_below(roots, scan_floor);
                let is_floor_reached = roots.len() < page_len;
                if let (Some(first), Some(last)) = (roots.first(), roots.last()) {
                    let start_index = u64::from(first.index.0);
                    let last_index = last.index.0;
                    let entries: Vec<(BlockHeight, [u8; 32])> = roots
                        .into_iter()
                        .map(|root| (root.completing_block_height, root.root_bytes))
                        .collect();
                    self.inner
                        .storage
                        .put_subtree_roots(protocol, start_index, entries)
                        .await?;
                    next_index = last_index.saturating_add(1);
                }
                if is_floor_reached || page_len < SUBTREE_ROOT_PAGE as usize {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Rewinds the wallet's derived state to exactly `rewind_to` using the chain's tree
    /// state at that height.
    ///
    /// One rung of the sync driver's repair ladder: truncates below a divergence via
    /// [`WalletStorage::truncate_to_chain_state`] (which lands the wallet at exactly the
    /// target height) and publishes [`WalletEvent::ReorgDetected`] so hosts observe the
    /// rollback. The next sync re-plans from the rewound frontier.
    pub(crate) async fn rewind_to_height(
        &self,
        chain: &dyn ChainSource,
        rewind_to: BlockHeight,
    ) -> Result<(), WalletError> {
        let chain_epoch = chain.current_epoch().await?;
        let chain_state = chain_state_at(chain, chain_epoch, rewind_to).await?;
        self.inner
            .storage
            .truncate_to_chain_state(chain_state)
            .await?;
        self.publish_event(WalletEvent::ReorgDetected {
            rolled_back_to_height: rewind_to,
            new_settled_tip_height: rewind_to,
        });
        Ok(())
    }

    fn emit_caught_up(
        &self,
        target_height: BlockHeight,
        transparent_utxo_count: u64,
    ) -> SyncOutcome {
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: target_height,
            target_height,
        });
        SyncOutcome {
            scanned_from_height: target_height,
            scanned_to_height: target_height,
            block_count: 0,
            transparent_utxo_count,
            completed_at_ms: current_unix_ms(),
        }
    }

    async fn scan_and_emit(
        &self,
        context: ScanContext,
        from_state: TreeStateArtifact,
        chain: &dyn ChainSource,
        chain_epoch: zally_chain::ChainEpoch,
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

        let transparent_utxo_count = self.sync_transparent_utxos(chain, chain_epoch).await?;

        if let Some(diverged_height) = self
            .verify_tree_roots(chain, chain_epoch, outcome.scanned_to_height)
            .await?
        {
            return Err(WalletError::TreeRootsDiverged {
                height: diverged_height,
            });
        }

        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: outcome.scanned_to_height,
            target_height,
        });
        Ok(SyncOutcome {
            scanned_from_height: scanned_from,
            scanned_to_height: outcome.scanned_to_height,
            block_count,
            transparent_utxo_count,
            completed_at_ms: current_unix_ms(),
        })
    }

    /// Checks the wallet's note-commitment tree roots against the chain's tree state at
    /// the just-scanned `height`, returning `Some(height)` on a proven divergence.
    ///
    /// A mismatch proves the wallet assembled a corrupt note-commitment tree, which the
    /// network rejects at spend time as an invalid shielded proof; a match clears the tree as
    /// the suspect and points at the proving inputs instead. The wallet roots are anchored at
    /// the latest retained checkpoint, which each scan creates at its final block, so they
    /// correspond to exactly `height`. Both sides decode roots little-endian, so the
    /// comparison is exact. Empty trees skip the comparison, but storage and chain-source
    /// failures propagate so an expired epoch pin restarts the complete sync attempt.
    async fn verify_tree_roots(
        &self,
        chain: &dyn ChainSource,
        chain_epoch: zally_chain::ChainEpoch,
        height: BlockHeight,
    ) -> Result<Option<BlockHeight>, WalletError> {
        let wallet_roots = self.inner.storage.commitment_tree_roots().await?;
        let chain_state = chain_state_at(chain, chain_epoch, height).await?;
        let chain_roots = self.inner.storage.tree_state_roots(chain_state).await?;
        let sapling_match = wallet_roots
            .sapling
            .zip(chain_roots.sapling)
            .map(|(wallet_root, chain_root)| wallet_root == chain_root);
        let orchard_match = wallet_roots
            .orchard
            .zip(chain_roots.orchard)
            .map(|(wallet_root, chain_root)| wallet_root == chain_root);
        let ironwood_match = wallet_roots
            .ironwood
            .zip(chain_roots.ironwood)
            .map(|(wallet_root, chain_root)| wallet_root == chain_root);

        match (sapling_match, orchard_match, ironwood_match) {
            (None, None, None) => {
                tracing::warn!(
                    target: "zally::sync",
                    event = "wallet_tree_root_check_skipped",
                    height = height.as_u32(),
                    "wallet commitment trees are empty"
                );
                Ok(None)
            }
            _ if sapling_match != Some(false)
                && orchard_match != Some(false)
                && ironwood_match != Some(false) =>
            {
                tracing::info!(
                    target: "zally::sync",
                    event = "wallet_tree_root_check",
                    height = height.as_u32(),
                    result = "match",
                    sapling_checked = sapling_match.is_some(),
                    orchard_checked = orchard_match.is_some(),
                    ironwood_checked = ironwood_match.is_some(),
                    "wallet commitment-tree roots agree with the chain"
                );
                Ok(None)
            }
            _ => {
                tracing::warn!(
                    target: "zally::sync",
                    event = "wallet_tree_root_check",
                    height = height.as_u32(),
                    result = "mismatch",
                    sapling_match = ?sapling_match,
                    orchard_match = ?orchard_match,
                    ironwood_match = ?ironwood_match,
                    wallet_sapling = %wallet_roots.sapling.map_or_else(String::new, hex::encode),
                    chain_sapling = %chain_roots.sapling.map_or_else(String::new, hex::encode),
                    wallet_orchard = %wallet_roots.orchard.map_or_else(String::new, hex::encode),
                    chain_orchard = %chain_roots.orchard.map_or_else(String::new, hex::encode),
                    wallet_ironwood = %wallet_roots.ironwood.map_or_else(String::new, hex::encode),
                    chain_ironwood = %chain_roots.ironwood.map_or_else(String::new, hex::encode),
                    "wallet commitment-tree roots diverge from the chain; spends will be rejected"
                );
                Ok(Some(height))
            }
        }
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

    async fn sync_transparent_utxos(
        &self,
        chain: &dyn ChainSource,
        chain_epoch: zally_chain::ChainEpoch,
    ) -> Result<u64, WalletError> {
        let receivers = self.inner.storage.list_transparent_receivers().await?;
        let mut receiver_batches = Vec::with_capacity(receivers.len());
        for receiver in receivers {
            let utxos = chain
                .transparent_utxos(chain_epoch, &receiver.script_pub_key_bytes)
                .await?;
            receiver_batches.push((receiver, utxos));
        }
        let transparent_utxo_rows =
            validate_transparent_utxo_batches(chain_epoch, receiver_batches)?;
        Ok(self
            .inner
            .storage
            .record_transparent_utxos(transparent_utxo_rows)
            .await?)
    }
}

fn validate_transparent_utxo_batches(
    chain_epoch: zally_chain::ChainEpoch,
    receiver_batches: Vec<(TransparentReceiverRow, Vec<TransparentUtxo>)>,
) -> Result<Vec<TransparentUtxoRow>, WalletError> {
    let mut outpoints = std::collections::HashSet::new();
    let mut transparent_utxo_rows = Vec::new();
    for (receiver, utxos) in receiver_batches {
        for utxo in utxos {
            if utxo.confirmed_at_height > chain_epoch.visible_tip().height {
                return Err(WalletError::ChainSource(
                    ChainSourceError::MalformedTransparentUtxoSet {
                        reason: format!(
                            "outpoint {}:{} is confirmed at {} above pinned visible tip {}",
                            utxo.tx_id,
                            utxo.output_index,
                            utxo.confirmed_at_height,
                            chain_epoch.visible_tip().height,
                        ),
                    },
                ));
            }
            if !outpoints.insert((utxo.tx_id, utxo.output_index)) {
                return Err(WalletError::ChainSource(
                    ChainSourceError::MalformedTransparentUtxoSet {
                        reason: format!(
                            "outpoint {}:{} appears more than once",
                            utxo.tx_id, utxo.output_index
                        ),
                    },
                ));
            }
            if utxo.script_pub_key_bytes != receiver.script_pub_key_bytes {
                return Err(WalletError::ChainSource(
                    ChainSourceError::MalformedTransparentUtxoSet {
                        reason: format!(
                            "outpoint {}:{} script did not match wallet receiver for account {:?}",
                            utxo.tx_id, utxo.output_index, receiver.account_id
                        ),
                    },
                ));
            }
            transparent_utxo_rows.push(TransparentUtxoRow::new(
                utxo.tx_id,
                utxo.output_index,
                utxo.value_zat,
                utxo.confirmed_at_height,
                utxo.script_pub_key_bytes,
            ));
        }
    }
    Ok(transparent_utxo_rows)
}

fn block_timestamp_index(blocks: &[CompactBlockArtifact]) -> HashMap<u32, u64> {
    blocks
        .iter()
        .map(|block| {
            let height = block.height.as_u32();
            let timestamp_ms = u64::from(block.block_time_seconds).saturating_mul(1_000);
            (height, timestamp_ms)
        })
        .collect()
}

const fn shielded_pool_for(protocol: zcash_protocol::ShieldedPool) -> ShieldedPool {
    match protocol {
        zcash_protocol::ShieldedPool::Sapling => ShieldedPool::Sapling,
        zcash_protocol::ShieldedPool::Orchard => ShieldedPool::Orchard,
        zcash_protocol::ShieldedPool::Ironwood => ShieldedPool::Ironwood,
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
    chain_epoch: zally_chain::ChainEpoch,
    start_height: BlockHeight,
    end_height: BlockHeight,
) -> Result<Vec<CompactBlockArtifact>, WalletError> {
    let range = BlockHeightRange::new(start_height, end_height).ok_or(WalletError::ChainSource(
        ChainSourceError::CompactBlockStreamOrder {
            expected_height: start_height,
            actual_height: Some(end_height),
        },
    ))?;
    let mut stream = chain.compact_blocks(chain_epoch, range).await?;
    let mut blocks = Vec::new();
    while let Some(stream_item) = stream.next().await {
        let block = stream_item?;
        let actual_height = Some(BlockHeight::from(u32::from(block.height)));
        let received_count = u64::try_from(blocks.len()).unwrap_or(u64::MAX);
        validate_compact_block_height(start_height, end_height, received_count, actual_height)?;
        blocks.push(block);
    }
    let received_count = u64::try_from(blocks.len()).unwrap_or(u64::MAX);
    validate_compact_block_count(start_height, end_height, received_count)?;
    Ok(blocks)
}

fn validate_compact_block_height(
    start_height: BlockHeight,
    end_height: BlockHeight,
    received_count: u64,
    actual_height: Option<BlockHeight>,
) -> Result<(), WalletError> {
    let expected_count = u64::from(end_height.as_u32())
        .saturating_sub(u64::from(start_height.as_u32()))
        .saturating_add(1);
    let expected_height = if received_count < expected_count {
        start_height
            .as_u32()
            .checked_add(u32::try_from(received_count).unwrap_or(u32::MAX))
            .map(BlockHeight::from)
    } else {
        None
    };
    if actual_height != expected_height {
        return Err(WalletError::ChainSource(
            ChainSourceError::CompactBlockStreamOrder {
                expected_height: expected_height.unwrap_or(end_height),
                actual_height,
            },
        ));
    }
    Ok(())
}

fn validate_compact_block_count(
    start_height: BlockHeight,
    end_height: BlockHeight,
    received_count: u64,
) -> Result<(), WalletError> {
    let expected_count = u64::from(end_height.as_u32())
        .saturating_sub(u64::from(start_height.as_u32()))
        .saturating_add(1);
    if received_count != expected_count {
        let missing_offset = u32::try_from(received_count).unwrap_or(u32::MAX);
        let expected_height = start_height
            .as_u32()
            .checked_add(missing_offset)
            .map_or(end_height, BlockHeight::from);
        return Err(WalletError::ChainSource(
            ChainSourceError::CompactBlockStreamOrder {
                expected_height,
                actual_height: None,
            },
        ));
    }
    Ok(())
}

/// Fetches the `ChainState` at exactly `height` (the note-commitment frontier after `height`).
///
/// Returns a [`ChainSourceError::MalformedCompactBlock`] when the tree-state bytes cannot be
/// decoded.
pub(crate) async fn chain_state_at(
    chain: &dyn ChainSource,
    chain_epoch: zally_chain::ChainEpoch,
    height: BlockHeight,
) -> Result<TreeStateArtifact, WalletError> {
    let tree_state = chain.tree_state_at(chain_epoch, height).await?;
    if tree_state.height != height {
        return Err(WalletError::ChainSource(
            ChainSourceError::TreeStateAnchorHeightMismatch {
                requested_height: height,
                returned_height: tree_state.height,
            },
        ));
    }
    Ok(tree_state)
}

/// Fetches the `ChainState` anchor immediately below `at_height`.
///
/// Shared by `sync_inner` (the `from_state` for a scan range), the wallet builder, and
/// [`Wallet::reset_to_birthday`] (the rebuild anchor below the birthday). The chain source
/// serves the tree state at the exact prior height.
pub(crate) async fn fetch_prior_chain_state(
    chain: &dyn ChainSource,
    chain_epoch: zally_chain::ChainEpoch,
    at_height: BlockHeight,
) -> Result<TreeStateArtifact, WalletError> {
    chain_state_at(
        chain,
        chain_epoch,
        BlockHeight::from(at_height.as_u32().saturating_sub(1)),
    )
    .await
}

/// Keeps the prefix of `roots` whose completing block sits at or below the wallet's fully
/// scanned height.
///
/// A subtree root completing above the scanned frontier folds leaves the wallet has not
/// scanned into a single node inside the shard the frontier occupies; installing it makes
/// any read spanning that shard commit to unscanned chain state. Completing heights grow
/// with the subtree index, so everything past the first too-new root is deferred to a later
/// backfill pass, once scanning crosses the shard boundary. A wallet with no fully scanned
/// height yet defers every root.
fn subtree_roots_completed_at_or_below(
    roots: Vec<zally_chain::SubtreeRoot>,
    scan_floor: Option<BlockHeight>,
) -> Vec<zally_chain::SubtreeRoot> {
    let Some(floor) = scan_floor else {
        return Vec::new();
    };
    let keep_count = roots
        .iter()
        .take_while(|root| root.completing_block_height <= floor)
        .count();
    let mut roots = roots;
    roots.truncate(keep_count);
    roots
}

fn validate_subtree_root_page(
    pool: ShieldedPool,
    requested_start: u32,
    requested_count: u32,
    roots: &[zally_chain::SubtreeRoot],
) -> Result<(), WalletError> {
    if roots.len() > requested_count as usize {
        return Err(WalletError::ChainSource(
            ChainSourceError::MalformedSubtreeRootPage {
                pool,
                reason: format!(
                    "returned {} roots for requested count {requested_count}",
                    roots.len()
                ),
            },
        ));
    }
    for (offset, root) in roots.iter().enumerate() {
        let expected_index =
            requested_start.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        if root.index.0 != expected_index {
            return Err(WalletError::ChainSource(
                ChainSourceError::MalformedSubtreeRootPage {
                    pool,
                    reason: format!("expected index {expected_index}, received {}", root.index.0),
                },
            ));
        }
    }
    if roots
        .windows(2)
        .any(|pair| pair[0].completing_block_height > pair[1].completing_block_height)
    {
        return Err(WalletError::ChainSource(
            ChainSourceError::MalformedSubtreeRootPage {
                pool,
                reason: "completing block heights must be nondecreasing".to_owned(),
            },
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit-test constructors use fixed values whose invalidity is a fixture bug"
)]
mod tests {
    use super::*;
    use zally_chain::{SubtreeIndex, SubtreeRoot};

    fn subtree_root_completing_at(index: u32, height: u32) -> SubtreeRoot {
        SubtreeRoot {
            index: SubtreeIndex(index),
            root_bytes: [u8::try_from(index).unwrap_or(u8::MAX); 32],
            completing_block_height: BlockHeight::from(height),
        }
    }

    fn transparent_test_epoch() -> zally_chain::ChainEpoch {
        let tip = zally_chain::BlockId {
            height: BlockHeight::from(10),
            hash: zally_core::BlockHash::from_bytes([10; 32]),
        };
        zally_chain::ChainEpoch::new(
            zally_chain::ChainEpochId::new(1),
            Network::regtest(),
            tip,
            tip,
        )
        .expect("test epoch is valid")
    }

    fn transparent_receiver(id: u128, script: &[u8]) -> TransparentReceiverRow {
        TransparentReceiverRow::new(
            zally_core::AccountId::from_uuid(uuid::Uuid::from_u128(id)),
            script.to_vec(),
        )
    }

    fn transparent_utxo(tx_byte: u8, script: &[u8], height: u32) -> TransparentUtxo {
        TransparentUtxo {
            tx_id: zally_core::TxId::from_bytes([tx_byte; 32]),
            output_index: 0,
            value_zat: zally_core::Zatoshis::try_from(1).unwrap_or(zally_core::Zatoshis::zero()),
            confirmed_at_height: BlockHeight::from(height),
            script_pub_key_bytes: script.to_vec(),
        }
    }

    #[test]
    fn later_malformed_receiver_yields_no_storage_batch() {
        let result = validate_transparent_utxo_batches(
            transparent_test_epoch(),
            vec![
                (
                    transparent_receiver(1, &[1]),
                    vec![transparent_utxo(1, &[1], 9)],
                ),
                (
                    transparent_receiver(2, &[2]),
                    vec![transparent_utxo(2, &[2], 11)],
                ),
            ],
        );
        assert!(matches!(
            result,
            Err(WalletError::ChainSource(
                ChainSourceError::MalformedTransparentUtxoSet { .. }
            ))
        ));
    }

    #[test]
    fn cross_receiver_duplicate_yields_no_storage_batch() {
        let duplicate = transparent_utxo(1, &[1], 9);
        let result = validate_transparent_utxo_batches(
            transparent_test_epoch(),
            vec![
                (transparent_receiver(1, &[1]), vec![duplicate.clone()]),
                (transparent_receiver(2, &[1]), vec![duplicate]),
            ],
        );
        assert!(matches!(
            result,
            Err(WalletError::ChainSource(
                ChainSourceError::MalformedTransparentUtxoSet { .. }
            ))
        ));
    }

    #[test]
    fn subtree_roots_completing_above_the_scan_floor_are_deferred() {
        let roots = vec![
            subtree_root_completing_at(0, 3_364_755),
            subtree_root_completing_at(1, 3_861_020),
            subtree_root_completing_at(2, 4_094_022),
        ];
        let kept = subtree_roots_completed_at_or_below(
            roots.clone(),
            Some(BlockHeight::from(4_050_200u32)),
        );
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[1].index, SubtreeIndex(1));

        let kept_at_boundary = subtree_roots_completed_at_or_below(
            roots.clone(),
            Some(BlockHeight::from(4_094_022u32)),
        );
        assert_eq!(kept_at_boundary.len(), 3);

        assert!(subtree_roots_completed_at_or_below(roots, None).is_empty());
    }

    #[test]
    fn subtree_root_pages_require_requested_bounds_and_ordering() {
        let pool = ShieldedPool::Orchard;
        assert!(
            validate_subtree_root_page(
                pool,
                3,
                2,
                &[
                    subtree_root_completing_at(3, 10),
                    subtree_root_completing_at(4, 11),
                ]
            )
            .is_ok()
        );
        for malformed in [
            vec![
                subtree_root_completing_at(3, 10),
                subtree_root_completing_at(5, 11),
            ],
            vec![
                subtree_root_completing_at(3, 11),
                subtree_root_completing_at(4, 10),
            ],
            vec![
                subtree_root_completing_at(3, 10),
                subtree_root_completing_at(4, 11),
                subtree_root_completing_at(5, 12),
            ],
        ] {
            assert!(matches!(
                validate_subtree_root_page(pool, 3, 2, &malformed),
                Err(WalletError::ChainSource(
                    ChainSourceError::MalformedSubtreeRootPage { .. }
                ))
            ));
        }
    }

    #[tokio::test]
    async fn chain_state_at_rejects_custom_source_height_mismatch() {
        let chain = zally_testkit::MockChainSource::new(Network::regtest());
        let handle = chain.handle();
        handle.advance_tip(BlockHeight::from(8));
        let chain_epoch = chain.current_epoch().await.expect("mock epoch");
        handle.serve_tree_state_for(
            BlockHeight::from(7),
            TreeStateArtifact {
                network: Network::regtest(),
                height: BlockHeight::from(8),
                block_hash: zally_core::BlockHash::from_bytes([8; 32]),
                block_time_seconds: 0,
                sapling_final_state_bytes: vec![1],
                orchard_final_state_bytes: vec![1],
                ironwood_final_state_bytes: Vec::new(),
            },
        );
        assert!(matches!(
            chain_state_at(&chain, chain_epoch, BlockHeight::from(7)).await,
            Err(WalletError::ChainSource(
                ChainSourceError::TreeStateAnchorHeightMismatch { .. }
            ))
        ));
    }

    #[test]
    fn compact_block_sequence_rejects_missing_duplicate_and_out_of_order_heights() {
        let start = BlockHeight::from(10);
        let end = BlockHeight::from(12);

        assert!(matches!(
            validate_compact_block_count(start, end, 2),
            Err(WalletError::ChainSource(
                ChainSourceError::CompactBlockStreamOrder {
                    expected_height,
                    actual_height: None,
                }
            )) if expected_height == BlockHeight::from(12)
        ));
        for actual_height in [BlockHeight::from(10), BlockHeight::from(12)] {
            assert!(matches!(
                validate_compact_block_height(start, end, 1, Some(actual_height)),
                Err(WalletError::ChainSource(
                    ChainSourceError::CompactBlockStreamOrder {
                        expected_height,
                        actual_height: Some(observed_height),
                    }
                )) if expected_height == BlockHeight::from(11)
                    && observed_height == actual_height
            ));
        }
    }

    #[test]
    fn compact_block_sequence_accepts_the_full_u32_height_domain() {
        let start = BlockHeight::from(0);
        let end = BlockHeight::from(u32::MAX);

        assert!(validate_compact_block_height(start, end, 0, Some(start)).is_ok());
        assert!(validate_compact_block_height(start, end, u64::from(u32::MAX), Some(end),).is_ok());
        assert!(validate_compact_block_count(start, end, u64::from(u32::MAX) + 1).is_ok());
    }

    #[test]
    fn named_cures_rewind_regardless_of_posture() {
        let faults = [
            WalletError::Storage(StorageError::CommitmentTreeConflict {
                reason: "subtree root mismatch".into(),
            }),
            WalletError::Storage(StorageError::ChainReorgDetected {
                at_height: BlockHeight::from(10),
            }),
            WalletError::TreeRootsDiverged {
                height: BlockHeight::from(10),
            },
        ];
        for fault in faults {
            assert_eq!(repair_for(&fault), SyncRepair::Rewind);
        }
    }

    #[test]
    fn retryable_faults_retry() {
        let fault = WalletError::CircuitBroken { operation: "test" };
        assert_eq!(fault.posture(), FailurePosture::Retryable);
        assert_eq!(repair_for(&fault), SyncRepair::Retry);
    }

    #[test]
    fn requires_operator_faults_park() {
        let faults = [
            WalletError::NetworkMismatch {
                storage: Network::Mainnet,
                requested: Network::Testnet,
            },
            WalletError::NoSealedSeed,
            WalletError::AccountNotFound,
            WalletError::SyncDriverFailed {
                reason: "panicked".into(),
            },
        ];
        for fault in faults {
            assert_eq!(fault.posture(), FailurePosture::RequiresOperator);
            assert_eq!(repair_for(&fault), SyncRepair::Park);
        }
    }

    #[test]
    fn not_retryable_faults_rewind() {
        let fault = WalletError::MemoOnTransparentRecipient;
        assert_eq!(fault.posture(), FailurePosture::NotRetryable);
        assert_eq!(repair_for(&fault), SyncRepair::Rewind);
    }

    fn one_strike_policy() -> SyncRecoveryPolicy {
        SyncRecoveryPolicy::default()
            .with_escalate_after_faults(1)
            .with_max_rescan_attempts(2)
    }

    /// Drives the recovery ladder through a stream of classified faults, mirroring the
    /// driver's record-then-apply interleaving, and records the `(rung, rewind_depth_index)`
    /// after each fault.
    fn drive_ladder(
        classifieds: impl IntoIterator<Item = SyncRepair>,
        policy: SyncRecoveryPolicy,
    ) -> Vec<(SyncRepair, usize)> {
        let mut recovery: Option<RecoveryState> = None;
        let mut ladder = Vec::new();
        for classified in classifieds {
            let recovery = recovery.get_or_insert_with(|| RecoveryState::entering(classified, 0));
            recovery.fold_fault(classified, policy);
            ladder.push((recovery.rung, recovery.rewind_depth_index));
            recovery.attempts_at_rung = recovery.attempts_at_rung.saturating_add(1);
        }
        ladder
    }

    #[test]
    fn faulted_iteration_with_progress_is_not_a_ladder_strike() {
        assert_eq!(
            height_delta(
                Some(BlockHeight::from(73_000)),
                Some(BlockHeight::from(74_000)),
            ),
            1_000
        );
        assert_eq!(height_delta(None, Some(BlockHeight::from(1_000))), 1_000);
        assert_eq!(
            height_delta(
                Some(BlockHeight::from(74_000)),
                Some(BlockHeight::from(74_000)),
            ),
            0
        );
        assert_eq!(height_delta(Some(BlockHeight::from(74_000)), None), 0);
        assert!(is_slow_progress(SyncRepair::Retry, 1_000));
        assert!(!is_slow_progress(SyncRepair::Retry, 0));
    }

    #[test]
    fn state_fault_with_progress_still_strikes_the_ladder() {
        assert!(!is_slow_progress(SyncRepair::Rewind, 1_000));
        assert!(!is_slow_progress(SyncRepair::Park, 1_000));

        let ladder = drive_ladder([SyncRepair::Rewind], one_strike_policy());
        assert_eq!(ladder, vec![(SyncRepair::Rewind, 0)]);
    }

    #[test]
    fn settling_past_the_fault_boundary_drops_the_ladder() {
        let mut recovery = RecoveryState::entering(SyncRepair::Rewind, 0);
        recovery.fault_height = Some(BlockHeight::from(4_148_826));
        let mut state = DriverState {
            recovery: Some(recovery),
            ..DriverState::default()
        };
        let recovered = state.settle_recovery(Some(BlockHeight::from(4_148_827)));
        assert!(recovered.is_some());
        assert!(state.recovery.is_none());
    }

    /// Reproduces the production wedge from issue #5.
    ///
    /// Each rewind re-covers a known-good range below the conflict, the trivially-completed
    /// sync must not clear the ladder, and the recurring fault must resume it so it
    /// eventually escalates past the rewind rungs.
    #[test]
    fn completed_sync_below_the_fault_boundary_keeps_ladder_memory() {
        let fault_height = BlockHeight::from(4_148_826);
        let policy = one_strike_policy();
        let mut state = DriverState::default();

        let mut rungs = Vec::new();
        for _ in 0..6 {
            // The recurring conflict at the same boundary.
            let recovery = state
                .recovery
                .get_or_insert_with(|| RecoveryState::entering(SyncRepair::Rewind, 0));
            recovery.dormant = false;
            recovery.fault_height = Some(
                recovery
                    .fault_height
                    .map_or(fault_height, |prior| prior.max(fault_height)),
            );
            recovery.fold_fault(SyncRepair::Rewind, policy);
            recovery.attempts_at_rung = recovery.attempts_at_rung.saturating_add(1);
            rungs.push((recovery.rung, recovery.rewind_depth_index));

            // The post-rewind verify range completes below the boundary.
            let recovered = state.settle_recovery(Some(BlockHeight::from(4_148_826)));
            assert!(
                recovered.is_none(),
                "a completed sync at the fault boundary must not clear the ladder"
            );
            assert!(
                state.recovery.as_ref().is_some_and(|r| r.dormant),
                "the retained ladder must be dormant between faults"
            );
        }

        assert!(
            rungs
                .iter()
                .any(|(rung, _)| *rung == SyncRepair::RescanFromBirthday),
            "the recurring conflict must escalate past the rewind rungs: {rungs:?}"
        );
    }

    #[test]
    fn a_fault_after_a_rewind_does_not_lower_the_recovery_bar() {
        let mut state = DriverState::default();
        let recovery = state
            .recovery
            .get_or_insert_with(|| RecoveryState::entering(SyncRepair::Rewind, 0));
        recovery.fault_height = Some(BlockHeight::from(4_148_826));
        // A fault observed at the rewound (lower) height keeps the original boundary.
        let lower = BlockHeight::from(4_148_816);
        recovery.fault_height = Some(
            recovery
                .fault_height
                .map_or(lower, |prior| prior.max(lower)),
        );
        assert_eq!(recovery.fault_height, Some(BlockHeight::from(4_148_826)));

        let recovered = state.settle_recovery(Some(BlockHeight::from(4_148_820)));
        assert!(recovered.is_none());
        assert!(state.recovery.is_some());
    }

    #[test]
    fn recovery_without_a_fault_height_clears_on_any_completed_sync() {
        let mut state = DriverState {
            recovery: Some(RecoveryState::entering(SyncRepair::Retry, 0)),
            ..DriverState::default()
        };
        let recovered = state.settle_recovery(Some(BlockHeight::from(1)));
        assert!(recovered.is_some());
        assert!(state.recovery.is_none());
    }

    #[test]
    fn environment_fault_streak_escalates_retry_to_park() {
        let ladder = drive_ladder([SyncRepair::Retry, SyncRepair::Retry], one_strike_policy());
        assert_eq!(ladder, vec![(SyncRepair::Retry, 0), (SyncRepair::Park, 0)]);
        assert!(
            !ladder.iter().any(|(rung, _)| matches!(
                rung,
                SyncRepair::Rewind | SyncRepair::RescanFromBirthday
            )),
            "an environment streak must never reach a state-repair rung"
        );
    }

    #[test]
    fn escalate_from_retry_parks_unless_a_state_fault_was_seen() {
        let mut environment = RecoveryState::entering(SyncRepair::Retry, 0);
        escalate(&mut environment);
        assert_eq!(environment.rung, SyncRepair::Park);

        let mut with_state_fault = RecoveryState::entering(SyncRepair::Retry, 0);
        with_state_fault.max_classified = SyncRepair::Rewind;
        escalate(&mut with_state_fault);
        assert_eq!(with_state_fault.rung, SyncRepair::Rewind);
        assert_eq!(with_state_fault.rewind_depth_index, 0);
    }

    #[test]
    fn state_fault_streak_walks_rewind_then_rescan_to_park() {
        let ladder = drive_ladder([SyncRepair::Rewind; 5], one_strike_policy());
        assert_eq!(ladder[0].0, SyncRepair::Rewind);
        assert_eq!(REWIND_LADDER_BLOCKS[ladder[0].1], 10);
        assert_eq!(ladder[1].0, SyncRepair::Rewind);
        assert_eq!(REWIND_LADDER_BLOCKS[ladder[1].1], 100);
        assert!(
            ladder
                .iter()
                .any(|(rung, _)| *rung == SyncRepair::RescanFromBirthday)
        );
        assert_eq!(ladder.last().map(|(rung, _)| *rung), Some(SyncRepair::Park));
    }

    #[test]
    fn mixed_streak_permits_rewind() {
        let ladder = drive_ladder(
            [
                SyncRepair::Retry,
                SyncRepair::Rewind,
                SyncRepair::Rewind,
                SyncRepair::Rewind,
                SyncRepair::Rewind,
                SyncRepair::Rewind,
            ],
            one_strike_policy(),
        );
        assert_eq!(ladder[0].0, SyncRepair::Retry);
        assert!(
            ladder.iter().any(|(rung, _)| *rung == SyncRepair::Rewind),
            "a corruption fault in the streak must permit the rewind rung"
        );
        assert_eq!(ladder.last().map(|(rung, _)| *rung), Some(SyncRepair::Park));
    }
}
