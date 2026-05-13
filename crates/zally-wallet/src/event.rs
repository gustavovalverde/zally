//! Wallet event stream.

use std::pin::Pin;

use futures_util::Stream;
use tokio_stream::wrappers::BroadcastStream;
use zally_chain::ShieldedPool;
use zally_core::{AccountId, BlockHeight, TxId};

/// Push notification from the wallet's sync loop.
///
/// Every `Wallet::sync` run emits [`WalletEvent::ScanProgress`] at the start and end of
/// the run; [`WalletEvent::ReorgDetected`] when the upstream chain rolls back; one
/// [`WalletEvent::TransactionConfirmed`] per newly confirmed wallet transaction; and one
/// [`WalletEvent::ShieldedReceiveObserved`] per newly observed shielded note that the
/// wallet owns. [`WalletEvent::Lagged`] is injected by the subscription stream when a
/// consumer drops events; the sync loop never emits it directly.
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
    /// A reorg rolled the wallet back; chain integration emits this when the upstream chain
    /// source signals a reorg over the wallet's scan range.
    ReorgDetected {
        /// Height the wallet's scan progress was rolled back to.
        rolled_back_to_height: BlockHeight,
        /// New visible tip after the reorg.
        new_tip_height: BlockHeight,
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
    ShieldedReceiveObserved {
        /// Account that owns the received note.
        account_id: AccountId,
        /// Transaction the note was created in.
        tx_id: TxId,
        /// Output index of the note within its transaction's bundle.
        output_index: u32,
        /// Note value in zatoshis.
        value_zat: u64,
        /// Block height the note was mined at.
        mined_height: BlockHeight,
        /// Block header timestamp in milliseconds (Unix epoch).
        block_timestamp_ms: u64,
        /// Shielded pool the note was created on.
        pool: ShieldedPool,
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
