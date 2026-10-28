//! Wallet event stream.

use std::pin::Pin;

use futures_util::Stream;
use tokio_stream::wrappers::BroadcastStream;
use zally_chain::ShieldedPool;
use zally_core::{AccountId, BlockHeight, HoldId, IdempotencyKey, TxId};

/// Push notification from the wallet's sync loop.
///
/// Every `Wallet::sync` run emits [`WalletEvent::ScanProgress`] at the start and end of
/// the run; one [`WalletEvent::TransactionConfirmed`] per newly confirmed wallet
/// transaction; and one [`WalletEvent::ShieldedReceiveObserved`] per newly observed shielded
/// note that the wallet owns.
///
/// A rewind emits [`WalletEvent::ReorgDetected`], then one
/// [`WalletEvent::TransactionReverted`] per wallet transaction and one
/// [`WalletEvent::ShieldedReceiveReverted`] per received note that left the chain. A reverted
/// transaction later produces [`WalletEvent::TransactionConfirmed`] again when the
/// replacement chain mines it, or [`WalletEvent::PendingBroadcastUnmineable`] once it can
/// never be mined.
///
/// [`WalletEvent::DerivedStateReset`] marks a wallet database rebuild from seed and birthday.
/// [`WalletEvent::TransparentUtxoRefreshFaulted`] marks a faulted transparent-UTXO refresh
/// attempt. [`WalletEvent::PendingBroadcastUnmineable`] names a broadcast to recreate, and
/// [`WalletEvent::HoldDispenseUnmineable`] names each dispense hold it had finalized.
/// [`WalletEvent::SourceHalted`] and [`WalletEvent::SourceRecovered`] mark the start and end
/// of a spend refusal over a halted chain source, and
/// [`WalletEvent::SourceReorgWindowTooShallow`] and [`WalletEvent::SourceReorgWindowSufficient`]
/// over the chain source's reorg window.
/// [`WalletEvent::Lagged`] is injected by the subscription
/// stream when a consumer drops events; the sync loop never emits it directly.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum WalletEvent {
    /// Scan progress: `scanned_height` advanced toward `target_height`.
    ScanProgress {
        /// Highest block the wallet has scanned so far.
        scanned_height: BlockHeight,
        /// Tip the wallet is catching up to.
        target_height: BlockHeight,
    },
    /// A reorg rolled the wallet back: the chain source announced it, or a scan found the
    /// wallet's blocks no longer chain onto the source's.
    ReorgDetected {
        /// Height the wallet's scan progress was rolled back to.
        rolled_back_to_height: BlockHeight,
        /// Settled tip of the chain epoch the rewind read its fork-point state from.
        new_settled_tip_height: BlockHeight,
    },
    /// A wallet transaction mined at `mined_height` left the chain in a rewind.
    ///
    /// The counterpart of [`Self::TransactionConfirmed`]. A transaction the wallet sent is
    /// submitted again while it can still be mined; the replacement chain either confirms it
    /// again or it becomes [`Self::PendingBroadcastUnmineable`].
    TransactionReverted {
        /// Reverted transaction identifier.
        tx_id: TxId,
        /// Height the transaction was mined at before the rewind.
        mined_height: BlockHeight,
    },
    /// A transaction belonging to the wallet was confirmed at `confirmed_at_height`.
    TransactionConfirmed {
        /// Confirmed transaction identifier.
        tx_id: TxId,
        /// Height at which it was confirmed.
        confirmed_at_height: BlockHeight,
    },
    /// A shielded note owned by `account_id` was observed at `mined_height` on `pool`.
    ///
    /// Emitted once per note as `Wallet::sync` advances past the block carrying the
    /// receive. `block_timestamp_ms` is the upstream `CompactBlock` header's mined
    /// timestamp converted to milliseconds; consumers should treat it as the authoritative
    /// receive time for window queries (instead of wall clock at observation).
    ///
    /// `is_change` and `spent_our_inputs` together let a consumer classify the receive
    /// without re-fetching the producing transaction. A self-funded change output sets
    /// both true; a transparent or shielded sweep of the wallet's own funds sets
    /// `spent_our_inputs` true without `is_change`; a third-party transfer leaves both
    /// false.
    ShieldedReceiveObserved {
        /// Account that owns the received note.
        account_id: AccountId,
        /// Transaction the note was created in.
        tx_id: TxId,
        /// Output index of the note within its transaction's bundle.
        output_index: u32,
        /// Note value.
        value_zat: zally_core::Zatoshis,
        /// Block height the note was mined at.
        mined_height: BlockHeight,
        /// Block header timestamp in milliseconds (Unix epoch).
        block_timestamp_ms: u64,
        /// Shielded pool the note was created on.
        pool: ShieldedPool,
        /// `zcash_client_sqlite` marked this note as change for the receiving account
        /// when the event was emitted. The flag can later move to true; see
        /// [`crate::ShieldedReceiveRecord::is_change`].
        is_change: bool,
        /// The producing transaction spent at least one input owned by the receiving
        /// account, across Sapling, Orchard, Ironwood, or transparent pools.
        spent_our_inputs: bool,
    },
    /// A shielded note previously reported by [`Self::ShieldedReceiveObserved`] left the
    /// chain in a rewind.
    ///
    /// A host that credited the receive must hold or reverse the credit. If the replacement
    /// chain mines the producing transaction again, the wallet reports the note again with
    /// [`Self::ShieldedReceiveObserved`].
    ShieldedReceiveReverted {
        /// Account that owned the received note.
        account_id: AccountId,
        /// Transaction the note was created in.
        tx_id: TxId,
        /// Output index of the note within its transaction's bundle.
        output_index: u32,
        /// Note value.
        value_zat: zally_core::Zatoshis,
        /// Height the note was mined at before the rewind.
        mined_height: BlockHeight,
        /// Shielded pool the note was created on.
        pool: ShieldedPool,
    },
    /// The wallet rebuilt its derived chain state from the seed and the account birthday.
    /// Chain state rebuilds by rescan; the payment ledger, including idempotency records
    /// and holds, survives.
    DerivedStateReset {
        /// Account recreated in the fresh database. Equal to the prior id: account
        /// identity is key identity, not database identity.
        account_id: AccountId,
    },
    /// A [`crate::Wallet::refresh_transparent_utxos`] attempt run by the driver's own refresh
    /// loop faulted, including a timed-out attempt. Emitted on every faulted attempt so a
    /// host observes a stall as it happens rather than only in logs; `consecutive_failures`
    /// tells the host whether this is an isolated blip or a sustained outage.
    TransparentUtxoRefreshFaulted {
        /// Consecutive faulted attempts up to and including this one.
        consecutive_failures: u32,
        /// Fault description safe for logs and status pages.
        reason: String,
    },
    /// A wallet-owned broadcast can never be mined. Emitted once per broadcast.
    ///
    /// The wallet dropped the pending inputs it recorded for it, and the idempotency key that
    /// produced it now builds a replacement on its next send. librustzcash keeps the
    /// broadcast's inputs out of new proposals until its expiry height passes.
    ///
    /// The broadcast expired, or it was signed for a consensus branch or with a version that
    /// the chain no longer accepts, for example because a network upgrade activated while it
    /// was pending. Signatures commit to the consensus branch, so signing it again cannot
    /// repair it: recreate the payment from a new proposal. The wallet judges this only from
    /// blocks it has scanned at or below the settled tip, so a reorg within the chain
    /// source's bound cannot bring the broadcast back.
    PendingBroadcastUnmineable {
        /// Transaction that can never be mined.
        tx_id: TxId,
        /// Why it can never be mined.
        reason: String,
    },
    /// A dispense hold was finalized with a transaction that can never be mined, so the
    /// dispense did not happen.
    ///
    /// Published right after [`Self::PendingBroadcastUnmineable`] for `tx_id`. When
    /// `is_reopened` is true the hold is active again and reserves its amount: dispense it
    /// again (its idempotency key now builds a replacement) and finalize it with the new
    /// transaction, or release it. When false, another active hold already holds
    /// `request_id`, so this one was released and that other hold carries the dispense.
    HoldDispenseUnmineable {
        /// The hold whose dispense did not happen.
        hold_id: HoldId,
        /// Request identifier the hold was reserved under.
        request_id: IdempotencyKey,
        /// Transaction the hold had been finalized with.
        tx_id: TxId,
        /// The hold is active again; false when it was released instead.
        is_reopened: bool,
    },
    /// The sync driver found that the chain source stopped following the chain, entered
    /// [`crate::SyncDriverPhase::SourceHalted`], and the wallet refuses new spends with
    /// [`crate::WalletError::ChainSourceHalted`]. [`Self::SourceRecovered`] follows once the
    /// source moves again.
    SourceHalted {
        /// Why the source is considered halted.
        cause: crate::SourceHalt,
        /// Unix milliseconds when the driver halted.
        since_ms: u64,
    },
    /// The chain source halted under [`Self::SourceHalted`] moves again: the driver left
    /// [`crate::SyncDriverPhase::SourceHalted`] and the wallet accepts new spends.
    ///
    /// Closing a halted driver also lifts the refusal, without this event.
    SourceRecovered,
    /// The sync driver read a chain source reorg window below
    /// [`crate::SyncDriverOptions::minimum_source_reorg_window_blocks`], so the wallet refuses
    /// new spends with [`crate::WalletError::SourceReorgWindowTooShallow`] and records no
    /// pending broadcast as unmineable.
    ///
    /// Emitted when the refusal starts, and again when the reported window changes while it
    /// stays too shallow. Raise the source's reorg window to at least `required_blocks`.
    SourceReorgWindowTooShallow {
        /// Reorg window the source reported.
        reorg_window_blocks: u32,
        /// Minimum window the sync driver requires.
        required_blocks: u32,
    },
    /// The sync driver read a chain source reorg window that meets its minimum again, so the
    /// hold reported by [`Self::SourceReorgWindowTooShallow`] is lifted.
    SourceReorgWindowSufficient {
        /// Reorg window the source reported.
        reorg_window_blocks: u32,
        /// Minimum window the sync driver requires.
        required_blocks: u32,
    },
    /// Consumer fell behind; `dropped_count` events were skipped before this notification.
    Lagged {
        /// Number of events dropped.
        dropped_count: u64,
    },
}

/// Async stream of wallet events.
///
/// Obtain one via [`crate::Wallet::observe`]. Dropping the stream unsubscribes; consumers
/// must drain quickly enough to avoid [`WalletEvent::Lagged`] notifications.
pub struct WalletEventStream {
    inner: Pin<Box<dyn Stream<Item = WalletEvent> + Send>>,
}

impl WalletEventStream {
    pub(crate) fn from_broadcast(receiver: tokio::sync::broadcast::Receiver<WalletEvent>) -> Self {
        use futures_util::StreamExt;
        let stream = BroadcastStream::new(receiver).filter_map(|delivery| async move {
            match delivery {
                Ok(event) => Some(event),
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    Some(WalletEvent::Lagged { dropped_count: n })
                }
            }
        });
        Self {
            inner: Box::pin(stream),
        }
    }

    /// Receives the next event. `None` when the wallet handle has dropped its broadcaster.
    pub async fn next(&mut self) -> Option<WalletEvent> {
        use futures_util::StreamExt;
        self.inner.next().await
    }
}
