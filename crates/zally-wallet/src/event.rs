//! Wallet event stream.

use std::pin::Pin;

use futures_util::Stream;
use tokio_stream::wrappers::BroadcastStream;
use zally_chain::ShieldedPool;
use zally_core::{AccountId, BlockHeight, TxId};

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
/// replacement chain mines it. Any sent transaction the settled chain passes the expiry
/// height of without mining it produces [`WalletEvent::TransactionExpired`].
///
/// [`WalletEvent::DerivedStateReset`] marks a wallet database rebuild from seed and birthday.
/// [`WalletEvent::TransparentUtxoRefreshFaulted`] marks a faulted transparent-UTXO refresh
/// attempt. [`WalletEvent::PendingBroadcastUnmineable`] names a broadcast to recreate.
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
    /// again or lets it expire.
    TransactionReverted {
        /// Reverted transaction identifier.
        tx_id: TxId,
        /// Height the transaction was mined at before the rewind.
        mined_height: BlockHeight,
    },
    /// A transaction the wallet sent can no longer be mined: the settled chain passed its
    /// expiry height without it.
    ///
    /// Its payment did not happen and the wallet spends its inputs again. The wallet
    /// releases the send's idempotency key at the same moment, so retrying the request with
    /// the same key creates a replacement payment. Until this event, a retry with the key
    /// returns this transaction, so a payment that can still be mined is never paid twice.
    TransactionExpired {
        /// Expired transaction identifier.
        tx_id: TxId,
        /// Last height at which the transaction could have been mined.
        expiry_height: BlockHeight,
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
