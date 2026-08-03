//! Wallet sync loop.
//!
//! `Wallet::sync` pins one source [`zally_chain::ChainEpoch`], selects a bounded scan range no
//! higher than its visible tip, and requests the exact predecessor [`TreeStateArtifact`]. The
//! chain source streams source-neutral compact blocks; the wallet validates their epoch and
//! range, then hands the blocks and anchor to [`zally_storage::WalletStorage::scan_blocks`]. The
//! storage boundary alone translates those values into librustzcash scan types and updates the
//! live wallet database.
//!
//! A drain opens with a `Wallet::sync` that records the chain tip, walks the ranges that
//! record queued through `Wallet::scan_queued_range` one bounded chunk per call, and closes
//! with a `Wallet::sync` that records the tip again. The exact predecessor tree artifact
//! anchors each chunk, so work is proportional to the new scan range rather than the full
//! chain height.
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
    SubtreeIndex,
};
use zally_core::{BlockHeight, CompactBlockArtifact, Network, TreeStateArtifact};
use zally_storage::{ScanRequest, StorageError};
use zcash_client_backend::data_api::scanning::ScanPriority;

use crate::error::WalletError;
use crate::event::WalletEvent;
use crate::retry::with_breaker_and_retry;
use crate::status::{SyncStatus, WalletStatus};
use crate::transparent_utxo_refresh::run_transparent_utxo_refresh_driver;
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
    /// Unix milliseconds when this run completed.
    pub completed_at_ms: u64,
}

/// The wallet's most recent checked observation of the chain.
///
/// An attempt earns one by writing what it read under its pinned epoch into the wallet
/// database and comparing the resulting commitment-tree roots against the chain. That pairing
/// is what a consumer gating spends can rely on, measuring age from [`Self::observed_at_ms`]
/// and distance from [`SyncSnapshot::lag_blocks`]: the wallet read the chain first-hand, and
/// the tree it will anchor a spend to agrees with it.
///
/// An attempt that commits nothing earns nothing. Neither does one whose chunk outran the
/// comparison, nor one whose fault classifies onto a state-repair rung: its blocks sit on a
/// view the wallet has just been told to distrust.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncObservation {
    /// Highest block height the wallet had committed when the observation was taken.
    pub scanned_to_height: BlockHeight,
    /// Unix milliseconds when the observation was taken.
    pub observed_at_ms: u64,
}

/// Self-healing policy for the [`SyncDriver`] repair ladder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncRecoveryPolicy {
    /// Consecutive faults at one ladder rung before the driver escalates to the next rung.
    /// Within [`SyncRepair::Rewind`] the same counter walks the rewind depth ladder.
    pub escalate_after_faults: u32,
    /// Consecutive faults tolerated at [`SyncRepair::Retry`] before escalating, when every
    /// fault since entering that rung classified as [`FailurePosture::Restartable`].
    ///
    /// A source that rotates its chain epoch answers precisely and is serving; the pin
    /// expiring under a running attempt is not evidence of backend trouble
    /// ([`FailurePosture::Restartable`]'s own contract). Escalating a healthy wallet to
    /// [`SyncRepair::Park`] on a short streak of rotations wedges it for no cause the ladder
    /// can cure, so this threshold is looser than [`Self::escalate_after_faults`]. A single
    /// fault that is not [`FailurePosture::Restartable`] falls back to the stricter
    /// threshold immediately: a rotation streak says nothing protective about a genuinely
    /// unreachable or misbehaving source.
    pub restartable_escalate_after_faults: u32,
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

    /// Returns the policy with `restartable_escalate_after_faults` replaced.
    #[must_use]
    pub const fn with_restartable_escalate_after_faults(
        self,
        restartable_escalate_after_faults: u32,
    ) -> Self {
        Self {
            restartable_escalate_after_faults,
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
            restartable_escalate_after_faults: self.restartable_escalate_after_faults.max(1),
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
            restartable_escalate_after_faults: 10,
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
    /// Minimum milliseconds between [`Wallet::refresh_transparent_utxos`] attempts.
    ///
    /// The refresh runs on its own loop, decoupled from the block-scan cadence above: a slow
    /// walk paces itself by its own duration, and this floor only prevents busy-looping when
    /// the walk returns quickly (an empty receiver list, or a source with no new UTXOs).
    pub transparent_utxo_refresh_interval_ms: u64,
    /// Maximum seconds one [`Wallet::refresh_transparent_utxos`] attempt may run before the
    /// refresh loop treats it as faulted and moves on to the next tick.
    ///
    /// Bounds a hung chain read the same way [`Self::sync_timeout_seconds`] bounds a hung
    /// scan attempt: without this, a source that never returns wedges the refresh loop
    /// silently forever, since the loop has no repair ladder of its own to notice.
    pub transparent_utxo_refresh_timeout_seconds: u64,
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

    /// Returns options with `transparent_utxo_refresh_interval_ms` replaced.
    #[must_use]
    pub const fn with_transparent_utxo_refresh_interval_ms(
        self,
        transparent_utxo_refresh_interval_ms: u64,
    ) -> Self {
        Self {
            transparent_utxo_refresh_interval_ms,
            ..self
        }
    }

    /// Returns options with `transparent_utxo_refresh_timeout_seconds` replaced.
    #[must_use]
    pub const fn with_transparent_utxo_refresh_timeout_seconds(
        self,
        transparent_utxo_refresh_timeout_seconds: u64,
    ) -> Self {
        Self {
            transparent_utxo_refresh_timeout_seconds,
            ..self
        }
    }

    fn normalized(self) -> Self {
        Self {
            poll_interval_ms: self.poll_interval_ms.max(1),
            max_sync_iterations_per_wake_count: self.max_sync_iterations_per_wake_count.max(1),
            sync_timeout_seconds: self.sync_timeout_seconds.max(1),
            recovery: self.recovery.normalized(),
            transparent_utxo_refresh_interval_ms: self.transparent_utxo_refresh_interval_ms.max(1),
            transparent_utxo_refresh_timeout_seconds: self
                .transparent_utxo_refresh_timeout_seconds
                .max(1),
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
            transparent_utxo_refresh_interval_ms: 5_000,
            transparent_utxo_refresh_timeout_seconds: 120,
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
    /// Most recent chain state the wallet committed and checked, and when.
    pub last_observation: Option<SyncObservation>,
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
            last_observation: None,
            last_fault: None,
            published_at_ms: current_unix_ms(),
        }
    }

    fn from_wallet_status(
        phase: SyncDriverPhase,
        wallet_status: &WalletStatus,
        last_observation: Option<SyncObservation>,
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
            last_observation,
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
    ///
    /// Alongside the block-scan loop this spawns a second, independent loop that refreshes
    /// transparent UTXOs on [`SyncDriverOptions::transparent_utxo_refresh_interval_ms`]'s
    /// cadence. The two loops share the wallet and chain source but never share an attempt:
    /// a slow or faulted refresh never blocks a scan iteration, and neither loop's epoch pin
    /// or retry ladder affects the other's.
    #[must_use]
    pub fn sync_continuously(self) -> SyncHandle {
        let (close_tx, close_rx) = oneshot::channel();
        let (status_tx, status_rx) = watch::channel(SyncSnapshot::starting(self.wallet.network()));
        let (transparent_utxo_refresh_close_tx, transparent_utxo_refresh_close_rx) =
            oneshot::channel();
        let transparent_utxo_refresh_join = tokio::spawn(run_transparent_utxo_refresh_driver(
            self.wallet.clone(),
            Arc::clone(&self.chain),
            self.options.transparent_utxo_refresh_interval_ms,
            self.options.transparent_utxo_refresh_timeout_seconds,
            transparent_utxo_refresh_close_rx,
        ));
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
            transparent_utxo_refresh_close_tx: Some(transparent_utxo_refresh_close_tx),
            transparent_utxo_refresh_join,
        }
    }
}

/// Handle returned by [`SyncDriver::sync_continuously`].
pub struct SyncHandle {
    close_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
    status_rx: watch::Receiver<SyncSnapshot>,
    transparent_utxo_refresh_close_tx: Option<oneshot::Sender<()>>,
    transparent_utxo_refresh_join: JoinHandle<()>,
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

    /// Requests shutdown and waits for both driver tasks to close.
    ///
    /// Neither driver task fails on its own, so the only close-time error is
    /// [`WalletError::SyncDriverFailed`] from a panic inside one of them.
    pub async fn close(mut self) -> Result<(), WalletError> {
        if let Some(close_tx) = self.close_tx.take() {
            let _ = close_tx.send(());
        }
        if let Some(close_tx) = self.transparent_utxo_refresh_close_tx.take() {
            let _ = close_tx.send(());
        }
        let sync_result = join_driver_task(self.join).await;
        let refresh_result = join_driver_task(self.transparent_utxo_refresh_join).await;
        sync_result.and(refresh_result)
    }
}

async fn join_driver_task(join: JoinHandle<()>) -> Result<(), WalletError> {
    match join.await {
        Ok(()) => Ok(()),
        Err(join_error) if join_error.is_panic() => Err(WalletError::SyncDriverFailed {
            reason: join_error.to_string(),
        }),
        Err(_cancelled) => Ok(()),
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
    last_observation: Option<SyncObservation>,
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

    /// Records a completed run as the wallet's latest chain observation.
    ///
    /// A run that scanned a chunk compared that chunk's commitment-tree roots against the
    /// chain before it returned. A run that scanned nothing compared nothing, so it re-dates
    /// the observation only up to a height the wallet has already checked; otherwise a
    /// caught-up cycle would launder an unchecked chunk into fresh agreement it never
    /// established.
    fn observe_completed_run(&mut self, outcome: SyncOutcome) {
        if outcome.block_count == 0 && !self.has_checked(outcome.scanned_to_height) {
            return;
        }
        self.last_observation = Some(SyncObservation {
            scanned_to_height: outcome.scanned_to_height,
            observed_at_ms: outcome.completed_at_ms,
        });
    }

    /// Records a faulted attempt's committed chunk as the wallet's latest chain observation.
    ///
    /// The wallet holds blocks it fetched under a pinned epoch and wrote through
    /// `scan_blocks`, and the fault landed after that write on a rung that says nothing about
    /// the wallet's state. Against a source that expires its pin inside every chunk's tail
    /// this is the only observation the driver ever produces.
    ///
    /// A chunk whose roots went uncompared is not one: its blocks are committed, but nothing
    /// has checked them against the chain, and a divergence the wallet never detected would
    /// read as fresh.
    fn observe_committed_chunk(&mut self, committed: CommittedChunk, frontier: ScanFrontier) {
        let (CommittedChunk::Checked, ScanFrontier::At(scanned_to_height)) = (committed, frontier)
        else {
            return;
        };
        self.last_observation = Some(SyncObservation {
            scanned_to_height,
            observed_at_ms: current_unix_ms(),
        });
    }

    /// Whether the wallet has compared its committed state against the chain at or above
    /// `height`.
    fn has_checked(&self, height: BlockHeight) -> bool {
        self.last_observation
            .is_some_and(|observation| observation.scanned_to_height >= height)
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
    /// Whether every fault folded into the current [`SyncRepair::Retry`] rung classified as
    /// [`FailurePosture::Restartable`]. Stale once the rung escalates past `Retry`.
    rung_all_restartable: bool,
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
            rung_all_restartable: true,
        }
    }

    /// Folds one classified fault into the ladder, escalating the rung when the current rung
    /// has exhausted its attempts. Returns the rung transition when one occurred.
    ///
    /// `restartable` marks a fault whose posture is [`FailurePosture::Restartable`]; it
    /// widens the escalation threshold at [`SyncRepair::Retry`] as long as every fault at
    /// that rung has carried the same posture (see
    /// [`SyncRecoveryPolicy::restartable_escalate_after_faults`]).
    fn fold_fault(
        &mut self,
        classified: SyncRepair,
        restartable: bool,
        policy: SyncRecoveryPolicy,
    ) -> Option<(SyncRepair, SyncRepair)> {
        self.max_classified = self.max_classified.max(classified);
        if classified > self.rung {
            self.rung_all_restartable = restartable;
        } else if classified == self.rung {
            self.rung_all_restartable = self.rung_all_restartable && restartable;
        }
        let escalation = if classified > self.rung {
            self.rung = classified;
            self.attempts_at_rung = 0;
            if classified == SyncRepair::Rewind {
                self.rewind_depth_index = 0;
            }
            None
        } else if self.attempts_at_rung
            >= escalation_threshold(self.rung, self.rung_all_restartable, policy)
        {
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
    Faulted {
        fault: ClassifiedFault,
        committed: CommittedChunk,
    },
}

/// A fault's rendered reason bundled with its ladder classification.
///
/// `restartable` marks a fault whose posture is [`FailurePosture::Restartable`]; see
/// [`RecoveryState::fold_fault`] for how it widens the escalation threshold.
struct ClassifiedFault {
    reason: String,
    repair: SyncRepair,
    restartable: bool,
}

impl ClassifiedFault {
    fn from_error(error: &WalletError) -> Self {
        Self {
            reason: error.to_string(),
            repair: repair_for(error),
            restartable: is_restartable_fault(error),
        }
    }
}

/// Whether an attempt's committed blocks carry a comparison against the chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommittedChunk {
    /// The attempt committed a chunk and compared its commitment-tree roots against the
    /// chain first, or committed nothing at all.
    Checked,
    /// The attempt committed a chunk and the comparison never ran.
    Unchecked,
}

/// The wallet's persisted scan frontier at one point in time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanFrontier {
    /// Highest block height the wallet has committed.
    At(BlockHeight),
    /// The wallet has committed no blocks.
    Empty,
    /// The status read failed, so the driver cannot say where the frontier sits.
    Unknown,
}

impl ScanFrontier {
    /// Block the frontier reached, `None` when the driver could not read it. A wallet that
    /// has committed nothing sits below every block.
    fn reached(self) -> Option<u32> {
        match self {
            Self::At(height) => Some(height.as_u32()),
            Self::Empty => Some(0),
            Self::Unknown => None,
        }
    }

    const fn height(self) -> Option<BlockHeight> {
        match self {
            Self::At(height) => Some(height),
            Self::Empty | Self::Unknown => None,
        }
    }
}

/// Whether this wakeup has already recorded the chain tip its scan queue is planned against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChainTipRecord {
    /// An iteration committed blocks, which proves the record landed ahead of them; the next
    /// iteration scans what the queue holds.
    Current,
    /// Nothing left below the record, nothing committed yet this wakeup, or a repair trimmed
    /// the queue to the rewound frontier; the next iteration records the tip first.
    Stale,
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

/// Drains the scan queue for one wakeup, bounded by
/// [`SyncDriverOptions::max_sync_iterations_per_wake_count`].
///
/// The first iteration records the chain tip, which queues every range between the wallet's
/// frontier and the chain; the rest scan what that record planned. Re-recording the tip
/// between chunks would re-queue a ten-block `Verify` range that outranks the bulk work, so
/// a wallet more than `PRUNING_DEPTH` blocks behind would advance ten blocks per chunk
/// forever. An iteration that committed blocks keeps the record even when it then faulted:
/// nothing but a repair removes queued ranges, and a source that expires its epoch pin
/// during every chunk's tail would otherwise re-queue that lookahead on every iteration.
/// Repairs do return the record to [`ChainTipRecord::Stale`], because truncation trims the
/// queue to the rewound frontier and only a fresh record re-queues the work above it.
///
/// Draining the queue is therefore not the same as reaching the chain: the queue drains
/// against a tip the wakeup may have observed many chunks ago. Only an iteration that both
/// recorded the tip and found nothing to scan ends the wakeup, so a caught-up report always
/// carries a tip the wallet just read and the spend path never builds an expiry height
/// against a tip the chain has passed.
async fn run_sync_wakeup(
    ctx: &DriverContext<'_>,
    close_rx: &mut oneshot::Receiver<()>,
    state: &mut DriverState,
) -> SyncWakeupExit {
    let mut chain_tip = ChainTipRecord::Stale;
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
            chain_tip = ChainTipRecord::Stale;
            if let Err(repair_error) = apply_repair(ctx, state).await {
                record_fault(ctx, state, ClassifiedFault::from_error(&repair_error), None).await;
                continue;
            }
        }
        let frontier_before = tokio::select! {
            biased;
            _ = &mut *close_rx => return SyncWakeupExit::CloseRequested,
            wallet_status = ctx.wallet.status_snapshot() => {
                let wallet_status = wallet_status.ok();
                let snapshot =
                    snapshot_for(ctx, SyncDriverPhase::Syncing, state, wallet_status.as_ref());
                publish_snapshot(ctx.status_tx, snapshot);
                scan_frontier(wallet_status.as_ref())
            }
        };
        let attempt = tokio::select! {
            biased;
            _ = &mut *close_rx => return SyncWakeupExit::CloseRequested,
            attempt = run_one_sync(ctx.wallet, ctx.chain, ctx.options, chain_tip) => attempt,
        };
        match attempt {
            SyncRunAttempt::Completed(outcome) => {
                let recorded_tip = chain_tip == ChainTipRecord::Stale;
                chain_tip = chain_tip_after_scan(outcome);
                if !complete_sync(ctx, state, outcome, recorded_tip).await {
                    return SyncWakeupExit::Reconciled;
                }
            }
            SyncRunAttempt::Faulted { fault, committed } => {
                let frontier_after =
                    scan_frontier(ctx.wallet.status_snapshot().await.ok().as_ref());
                let blocks_advanced = height_delta(frontier_before, frontier_after);
                chain_tip = chain_tip_after_fault(chain_tip, blocks_advanced);
                if is_slow_progress(fault.repair, blocks_advanced) {
                    state.observe_committed_chunk(committed, frontier_after);
                    note_slow_progress(
                        ctx,
                        state,
                        fault.reason,
                        blocks_advanced,
                        frontier_after.height(),
                    )
                    .await;
                } else {
                    record_fault(ctx, state, fault, frontier_after.height()).await;
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
    recorded_tip: bool,
) -> bool {
    let recovered = state.settle_recovery(Some(outcome.scanned_to_height));
    state.observe_completed_run(outcome);
    state.last_fault = None;
    let should_continue = should_continue_syncing(outcome, recorded_tip);
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

/// Reads the wallet's persisted scan frontier out of a wallet-status read.
fn scan_frontier(wallet_status: Option<&WalletStatus>) -> ScanFrontier {
    wallet_status.map_or(ScanFrontier::Unknown, |status| {
        status
            .scanned_height
            .map_or(ScanFrontier::Empty, ScanFrontier::At)
    })
}

/// Blocks the wallet's persisted scan frontier advanced between two reads.
///
/// A frontier the driver could not read yields zero. It is not progress the driver may
/// claim, and standing in the last published height for it would credit an attempt that
/// committed nothing with every block the wallet had ever scanned.
fn height_delta(before: ScanFrontier, after: ScanFrontier) -> u32 {
    match (before.reached(), after.reached()) {
        (Some(before), Some(after)) => after.saturating_sub(before),
        _ => 0,
    }
}

/// Whether a faulted iteration counts as slow progress instead of a ladder strike.
///
/// Only environment faults (classified [`SyncRepair::Retry`]) are excused by forward
/// progress. A state fault after a committed chunk still proves divergence; skipping its
/// repair would scan the next chunk on top of the corrupt state.
const fn is_slow_progress(repair: SyncRepair, blocks_advanced: u32) -> bool {
    matches!(repair, SyncRepair::Retry) && blocks_advanced > 0
}

/// Whether a completed iteration leaves this wakeup's chain-tip record standing.
///
/// A chunk of blocks leaves the rest of the queue below the same record. A drained queue
/// leaves nothing below it, so the next iteration records again and either finds blocks the
/// chain added during the drain or reports caught up against a tip it just read.
const fn chain_tip_after_scan(outcome: SyncOutcome) -> ChainTipRecord {
    if outcome.block_count > 0 {
        ChainTipRecord::Current
    } else {
        ChainTipRecord::Stale
    }
}

/// Whether a faulted iteration leaves this wakeup's chain-tip record standing.
///
/// Committed blocks prove the record landed ahead of them, and nothing but a repair removes
/// queued ranges. A source that expires its epoch pin during every chunk's tail would
/// otherwise re-record on every iteration, re-queueing the `Verify` lookahead each time and
/// capping a far-behind wallet at ten blocks per chunk.
const fn chain_tip_after_fault(recorded: ChainTipRecord, blocks_advanced: u32) -> ChainTipRecord {
    if blocks_advanced > 0 {
        ChainTipRecord::Current
    } else {
        recorded
    }
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
    classified: ClassifiedFault,
    fault_height: Option<BlockHeight>,
) {
    let now = current_unix_ms();
    let policy = ctx.options.recovery;
    let recovery = state
        .recovery
        .get_or_insert_with(|| RecoveryState::entering(classified.repair, now));
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
    let escalation = recovery.fold_fault(classified.repair, classified.restartable, policy);
    let fault = SyncFault {
        reason: classified.reason,
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

const fn escalation_threshold(
    rung: SyncRepair,
    rung_all_restartable: bool,
    policy: SyncRecoveryPolicy,
) -> u32 {
    match rung {
        SyncRepair::RescanFromBirthday => policy.max_rescan_attempts,
        SyncRepair::Retry if rung_all_restartable => policy.restartable_escalate_after_faults,
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
/// transient faults and expired source boundaries retry, operator dead ends park (the
/// literal [`FailurePosture::RequiresOperator`] definition), and the rest rewind: the ladder
/// escalates to a rebuild when rewinding does not cure, which is the self-healing default
/// for unknown corruption. An unverified chunk classifies on whatever stopped its
/// comparison.
fn repair_for(error: &WalletError) -> SyncRepair {
    let error = if let WalletError::UnverifiedScanChunk { source, .. } = error {
        source.as_ref()
    } else {
        error
    };
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
            FailurePosture::Retryable | FailurePosture::Restartable => SyncRepair::Retry,
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
            state.last_observation,
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
    chain_tip: ChainTipRecord,
) -> SyncRunAttempt {
    let attempt = async {
        match chain_tip {
            ChainTipRecord::Stale => wallet.sync(chain).await,
            ChainTipRecord::Current => wallet.scan_queued_range(chain).await,
        }
    };
    match timeout(Duration::from_secs(options.sync_timeout_seconds), attempt).await {
        Ok(Ok(outcome)) => SyncRunAttempt::Completed(outcome),
        Ok(Err(error)) => SyncRunAttempt::Faulted {
            committed: committed_chunk_for(&error),
            fault: ClassifiedFault::from_error(&error),
        },
        // A deadline can cut between a chunk reaching storage and the comparison checking it.
        // A stuck deadline is not the benign, self-healing shape of an expired boundary, so
        // it does not earn the wider restartable threshold.
        Err(_elapsed) => SyncRunAttempt::Faulted {
            fault: ClassifiedFault {
                reason: format!("sync exceeded {} seconds", options.sync_timeout_seconds),
                repair: SyncRepair::Retry,
                restartable: false,
            },
            committed: CommittedChunk::Unchecked,
        },
    }
}

/// Whether a faulted attempt left the wallet holding blocks it never compared to the chain.
const fn committed_chunk_for(error: &WalletError) -> CommittedChunk {
    if matches!(error, WalletError::UnverifiedScanChunk { .. }) {
        CommittedChunk::Unchecked
    } else {
        CommittedChunk::Checked
    }
}

/// Whether a fault classifies as [`FailurePosture::Restartable`].
///
/// Only meaningful when [`repair_for`] classified the same error as [`SyncRepair::Retry`]:
/// every named state-repair cure in [`repair_for`] always classifies to a higher rung
/// regardless of posture, so this can only be `true` there when the fault is a rotated
/// source boundary.
fn is_restartable_fault(error: &WalletError) -> bool {
    matches!(error.posture(), FailurePosture::Restartable)
}

/// Whether the driver should run another sync iteration in this wakeup.
///
/// A cycle that scanned a chunk (`block_count > 0`) leaves more scan-queue work. A cycle
/// that scanned none has drained the queue, which only means caught up when that same cycle
/// recorded the tip the queue was planned against; a drained
/// [`Wallet::scan_queued_range`] proves nothing about blocks mined since the record, so the
/// wakeup records once more before it stops.
const fn should_continue_syncing(outcome: SyncOutcome, recorded_tip: bool) -> bool {
    outcome.block_count > 0 || !recorded_tip
}

async fn build_snapshot(
    ctx: &DriverContext<'_>,
    phase: SyncDriverPhase,
    state: &DriverState,
) -> SyncSnapshot {
    let wallet_status = ctx.wallet.status_snapshot().await.ok();
    snapshot_for(ctx, phase, state, wallet_status.as_ref())
}

/// Builds a snapshot from a wallet-status read, falling back to the previously published
/// snapshot when the read failed (the driver must keep publishing regardless).
fn snapshot_for(
    ctx: &DriverContext<'_>,
    phase: SyncDriverPhase,
    state: &DriverState,
    wallet_status: Option<&WalletStatus>,
) -> SyncSnapshot {
    let Some(wallet_status) = wallet_status else {
        let mut snapshot = ctx.status_tx.borrow().clone();
        snapshot.phase = phase;
        snapshot.last_observation = state.last_observation;
        snapshot.last_fault.clone_from(&state.last_fault);
        return snapshot;
    };
    SyncSnapshot::from_wallet_status(
        phase,
        wallet_status,
        state.last_observation,
        state.last_fault.clone(),
    )
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
    /// Records the current chain tip, then advances the wallet by one bounded scan step
    /// toward the pinned epoch's visible tip.
    ///
    /// Recording the tip is what plans work: it queues the ranges between the wallet's
    /// scanned frontier and the chain, and it sets the tip that transaction expiry heights
    /// are computed against. The step then primes commitment-tree subtree roots and scans
    /// the highest-priority range `suggest_scan_ranges` returns, chunked to
    /// `MAX_BLOCKS_PER_SYNC`. Subtree roots let the wallet witness a note from its subtree
    /// root without scanning every block, so spendability does not require a full linear
    /// scan.
    ///
    /// A caller draining a backlog calls this once and then [`Wallet::scan_queued_range`]
    /// until a run reports no blocks: re-recording the tip between chunks re-queues a
    /// tip-adjacent `Verify` range that outranks the bulk work and caps every chunk at ten
    /// blocks. It then calls this once more, because only a run that recorded the tip and
    /// found nothing to scan proves the wallet reached the chain. The [`SyncDriver`] drains
    /// that way.
    ///
    /// Reorg safety comes from the spend-time confirmation depth (ZIP 315); scan-time
    /// divergences (`ChainReorgDetected`, `CommitmentTreeConflict`,
    /// [`WalletError::TreeRootsDiverged`]) surface as errors that the [`SyncDriver`] repairs
    /// by rewinding or rebuilding derived state.
    ///
    /// Fails closed on network mismatch. Emits `ScanProgress` events at the start and end of
    /// the run; per-block events are emitted by the storage scanner.
    ///
    /// `requires_operator` on network mismatch. `retryable` on transient chain-source
    /// failures.
    pub async fn sync(&self, chain: &dyn ChainSource) -> Result<SyncOutcome, WalletError> {
        self.run_scan_attempt("sync.attempt", || self.sync_inner(chain))
            .await
    }

    /// Advances the wallet by one bounded scan step through work the scan queue already
    /// holds, leaving the recorded chain tip alone.
    ///
    /// Companion to [`Wallet::sync`] for draining a backlog: the queue holds every range
    /// between the frontier and the tip recorded by the preceding `sync`, so successive
    /// calls walk it in `MAX_BLOCKS_PER_SYNC` chunks. A run that reports no blocks means the
    /// queue is drained at or below that recorded tip, which the chain may have passed
    /// during the drain. It is not a caught-up report and callers must not treat it as one:
    /// resume with [`Wallet::sync`] to record the tip again and learn about newer blocks.
    ///
    /// `requires_operator` on network mismatch. `retryable` on transient chain-source
    /// failures.
    pub async fn scan_queued_range(
        &self,
        chain: &dyn ChainSource,
    ) -> Result<SyncOutcome, WalletError> {
        self.run_scan_attempt("sync.scan_queued_range", || {
            self.scan_queued_range_inner(chain)
        })
        .await
    }

    /// Runs one breaker-guarded scan attempt and retires the broadcasts it aged out.
    ///
    /// One attempt is one unit of breaker accounting: splitting the chain-tip read into its
    /// own guarded call would record a success for every attempt whose scan then failed,
    /// and the failure streak would never reach the threshold.
    async fn run_scan_attempt<F, Fut>(
        &self,
        operation_label: &'static str,
        attempt: F,
    ) -> Result<SyncOutcome, WalletError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<SyncOutcome, WalletError>>,
    {
        let outcome = with_breaker_and_retry(
            &self.inner.circuit_breaker,
            self.retry_policy(),
            operation_label,
            attempt,
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
        let chain_epoch = self.pin_chain_epoch(chain).await?;
        let visible_tip = chain_epoch.visible_tip().height;
        self.inner.storage.update_chain_tip(visible_tip).await?;
        self.inner
            .storage
            .record_chain_tips(visible_tip, chain_epoch.settled_tip().height)
            .await?;
        self.scan_one_range(chain, chain_epoch).await
    }

    async fn scan_queued_range_inner(
        &self,
        chain: &dyn ChainSource,
    ) -> Result<SyncOutcome, WalletError> {
        let chain_epoch = self.pin_chain_epoch(chain).await?;
        self.scan_one_range(chain, chain_epoch).await
    }

    /// Pins one source epoch, failing closed when the source serves another network.
    pub(crate) async fn pin_chain_epoch(
        &self,
        chain: &dyn ChainSource,
    ) -> Result<zally_chain::ChainEpoch, WalletError> {
        if chain.network() != self.network() {
            return Err(WalletError::NetworkMismatch {
                storage: self.network(),
                requested: chain.network(),
            });
        }
        Ok(chain.current_epoch().await?)
    }

    async fn scan_one_range(
        &self,
        chain: &dyn ChainSource,
        chain_epoch: zally_chain::ChainEpoch,
    ) -> Result<SyncOutcome, WalletError> {
        let visible_tip = chain_epoch.visible_tip().height;
        let settled_tip = chain_epoch.settled_tip().height;
        let Some((scan_start, scan_end, priority)) = self
            .plan_scan_range(chain, chain_epoch, visible_tip)
            .await?
        else {
            return Ok(self.emit_caught_up(visible_tip));
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
    /// The queue is planned against the tip [`Wallet::sync`] records, and every range is
    /// clamped to the visible tip pinned here. Keeping the upstream chain tip and scan
    /// frontier aligned prevents an unscanned settlement-window gap from making notes in an
    /// incomplete commitment-tree shard appear unspendable.
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

    fn emit_caught_up(&self, target_height: BlockHeight) -> SyncOutcome {
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: target_height,
            target_height,
        });
        SyncOutcome {
            scanned_from_height: target_height,
            scanned_to_height: target_height,
            block_count: 0,
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

        // The only detector for a corrupt tree runs ahead of every read that can outlive the
        // epoch pin.
        if let Some(diverged_height) = self
            .verify_tree_roots(chain, chain_epoch, outcome.scanned_to_height)
            .await
            .map_err(|error| WalletError::UnverifiedScanChunk {
                scanned_to: outcome.scanned_to_height,
                source: Box::new(error),
            })?
        {
            return Err(WalletError::TreeRootsDiverged {
                height: diverged_height,
            });
        }

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

        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: outcome.scanned_to_height,
            target_height,
        });
        Ok(SyncOutcome {
            scanned_from_height: scanned_from,
            scanned_to_height: outcome.scanned_to_height,
            block_count,
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
                    reason = "wallet_trees_empty",
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

    /// Drives the recovery ladder through a stream of classified faults.
    ///
    /// Mirrors the driver's record-then-apply interleaving, and records the `(rung,
    /// rewind_depth_index)` after each fault. Every fault is treated as non-restartable; see
    /// `restartable_streak_gets_the_wider_threshold` for the restartable-specific ladder
    /// behaviour.
    fn drive_ladder(
        classifieds: impl IntoIterator<Item = SyncRepair>,
        policy: SyncRecoveryPolicy,
    ) -> Vec<(SyncRepair, usize)> {
        let mut recovery: Option<RecoveryState> = None;
        let mut ladder = Vec::new();
        for classified in classifieds {
            let recovery = recovery.get_or_insert_with(|| RecoveryState::entering(classified, 0));
            recovery.fold_fault(classified, false, policy);
            ladder.push((recovery.rung, recovery.rewind_depth_index));
            recovery.attempts_at_rung = recovery.attempts_at_rung.saturating_add(1);
        }
        ladder
    }

    #[test]
    fn faulted_iteration_with_progress_is_not_a_ladder_strike() {
        assert_eq!(
            height_delta(
                ScanFrontier::At(BlockHeight::from(73_000)),
                ScanFrontier::At(BlockHeight::from(74_000)),
            ),
            1_000
        );
        assert_eq!(
            height_delta(
                ScanFrontier::At(BlockHeight::from(74_000)),
                ScanFrontier::At(BlockHeight::from(74_000)),
            ),
            0
        );
        assert!(is_slow_progress(SyncRepair::Retry, 1_000));
        assert!(!is_slow_progress(SyncRepair::Retry, 0));
    }

    fn outcome_scanning(block_count: u64) -> SyncOutcome {
        SyncOutcome {
            scanned_from_height: BlockHeight::from(74_000),
            scanned_to_height: BlockHeight::from(75_000),
            block_count,
            completed_at_ms: 0,
        }
    }

    #[test]
    fn only_a_recording_iteration_may_report_the_wallet_caught_up() {
        assert!(
            !should_continue_syncing(outcome_scanning(0), true),
            "a recorded tip with nothing left below it is the caught-up report"
        );
        assert!(
            should_continue_syncing(outcome_scanning(0), false),
            "a drained queue under a tip recorded chunks ago says nothing about the chain now"
        );
        assert!(should_continue_syncing(outcome_scanning(1_000), true));
        assert!(should_continue_syncing(outcome_scanning(1_000), false));
    }

    #[test]
    fn a_drained_queue_sends_the_next_iteration_back_to_the_chain_tip() {
        assert_eq!(
            chain_tip_after_scan(outcome_scanning(1_000)),
            ChainTipRecord::Current,
            "the rest of the queue sits below the same record"
        );
        assert_eq!(
            chain_tip_after_scan(outcome_scanning(0)),
            ChainTipRecord::Stale,
            "an empty queue leaves nothing below the record to scan"
        );
    }

    #[test]
    fn a_committed_chunk_keeps_the_chain_tip_record_through_its_fault() {
        assert_eq!(
            chain_tip_after_fault(ChainTipRecord::Stale, 1_000),
            ChainTipRecord::Current,
            "a chunk that committed blocks proves the record landed ahead of them"
        );
        assert_eq!(
            chain_tip_after_fault(ChainTipRecord::Current, 1_000),
            ChainTipRecord::Current
        );
        assert_eq!(
            chain_tip_after_fault(ChainTipRecord::Stale, 0),
            ChainTipRecord::Stale,
            "an attempt that committed nothing proves nothing about the record"
        );
        assert_eq!(
            chain_tip_after_fault(ChainTipRecord::Current, 0),
            ChainTipRecord::Current,
            "a fault removes no queued range, so a standing record still stands"
        );
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
            recovery.fold_fault(SyncRepair::Rewind, false, policy);
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
    fn restartable_streak_gets_the_wider_threshold() {
        let policy = SyncRecoveryPolicy::default()
            .with_escalate_after_faults(1)
            .with_restartable_escalate_after_faults(3);
        let mut recovery = RecoveryState::entering(SyncRepair::Retry, 0);
        for _ in 0..3 {
            let escalation = recovery.fold_fault(SyncRepair::Retry, true, policy);
            assert_eq!(
                escalation, None,
                "a rotation streak below the wider threshold must not escalate"
            );
            recovery.attempts_at_rung = recovery.attempts_at_rung.saturating_add(1);
        }
        let escalation = recovery.fold_fault(SyncRepair::Retry, true, policy);
        assert_eq!(
            escalation,
            Some((SyncRepair::Retry, SyncRepair::Park)),
            "the streak must still escalate once it reaches the wider threshold"
        );
    }

    #[test]
    fn a_single_non_restartable_fault_reverts_to_the_strict_threshold() {
        let policy = SyncRecoveryPolicy::default()
            .with_escalate_after_faults(1)
            .with_restartable_escalate_after_faults(10);
        let mut recovery = RecoveryState::entering(SyncRepair::Retry, 0);
        let escalation = recovery.fold_fault(SyncRepair::Retry, true, policy);
        assert_eq!(escalation, None);
        recovery.attempts_at_rung = recovery.attempts_at_rung.saturating_add(1);

        let escalation = recovery.fold_fault(SyncRepair::Retry, false, policy);
        assert_eq!(
            escalation,
            Some((SyncRepair::Retry, SyncRepair::Park)),
            "a genuine retryable fault must not inherit the rotation streak's leniency"
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

    fn caught_up_outcome(at_height: u32) -> SyncOutcome {
        SyncOutcome {
            scanned_from_height: BlockHeight::from(at_height),
            scanned_to_height: BlockHeight::from(at_height),
            block_count: 0,
            completed_at_ms: 1,
        }
    }

    #[test]
    fn an_unreadable_frontier_is_never_progress() {
        assert_eq!(
            height_delta(
                ScanFrontier::Unknown,
                ScanFrontier::At(BlockHeight::from(4_202_991))
            ),
            0
        );
        assert_eq!(
            height_delta(
                ScanFrontier::At(BlockHeight::from(4_202_991)),
                ScanFrontier::Unknown
            ),
            0
        );
        assert_eq!(
            height_delta(
                ScanFrontier::Empty,
                ScanFrontier::At(BlockHeight::from(4_202_991))
            ),
            4_202_991,
            "a wallet that had committed nothing did commit this chunk"
        );
        assert_eq!(
            height_delta(
                ScanFrontier::At(BlockHeight::from(10)),
                ScanFrontier::At(BlockHeight::from(15))
            ),
            5
        );
    }

    #[test]
    fn an_unchecked_chunk_publishes_no_observation() {
        let mut state = DriverState::default();
        state.observe_committed_chunk(
            CommittedChunk::Unchecked,
            ScanFrontier::At(BlockHeight::from(50)),
        );
        assert_eq!(state.last_observation, None);

        state.observe_committed_chunk(
            CommittedChunk::Checked,
            ScanFrontier::At(BlockHeight::from(50)),
        );
        assert_eq!(
            state.last_observation.map(|o| o.scanned_to_height),
            Some(BlockHeight::from(50))
        );
    }

    #[test]
    fn a_caught_up_run_re_dates_only_a_checked_height() {
        let mut state = DriverState::default();
        state.observe_completed_run(caught_up_outcome(50));
        assert_eq!(
            state.last_observation, None,
            "nothing has compared the state this run reports as caught up"
        );

        state.observe_committed_chunk(
            CommittedChunk::Checked,
            ScanFrontier::At(BlockHeight::from(50)),
        );
        state.observe_completed_run(caught_up_outcome(50));
        assert_eq!(
            state.last_observation.map(|o| o.observed_at_ms),
            Some(1),
            "the caught-up run re-dates a height the wallet has checked"
        );
    }

    #[test]
    fn an_unverified_chunk_classifies_on_what_stopped_its_comparison() {
        let error = WalletError::UnverifiedScanChunk {
            scanned_to: BlockHeight::from(50),
            source: Box::new(WalletError::ChainSource(
                ChainSourceError::ChainEpochPinUnavailable,
            )),
        };
        assert_eq!(repair_for(&error), SyncRepair::Retry);
        assert_eq!(committed_chunk_for(&error), CommittedChunk::Unchecked);

        let diverged = WalletError::TreeRootsDiverged {
            height: BlockHeight::from(50),
        };
        assert_eq!(repair_for(&diverged), SyncRepair::Rewind);
        assert_eq!(committed_chunk_for(&diverged), CommittedChunk::Checked);
    }
}
