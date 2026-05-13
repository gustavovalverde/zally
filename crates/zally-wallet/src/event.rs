//! Wallet event stream.

use std::pin::Pin;

use futures_util::Stream;
use tokio_stream::wrappers::BroadcastStream;
use zally_chain::ShieldedPool;
use zally_core::{AccountId, BlockHeight, TxId};

/// Push notification from the wallet's sync loop.
///
/// Slice 2 emits [`WalletEvent::ScanProgress`], [`WalletEvent::ReorgDetected`], and
/// [`WalletEvent::Lagged`]. Slice 5 adds [`WalletEvent::TransactionConfirmed`] and
/// [`WalletEvent::ReceiverObserved`] when block scanning lands.
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
    /// Emitted by Slice 5's scan loop.
    TransactionConfirmed {
        /// Confirmed transaction identifier.
        tx_id: TxId,
        /// Height at which it was confirmed.
        confirmed_at_height: BlockHeight,
    },
    /// A note for `account_id` was observed at `seen_at_height` on `pool`. Emitted by
    /// Slice 5's scan loop.
    ReceiverObserved {
        /// Account whose receive observed the note.
        account_id: AccountId,
        /// Block height the note was found in.
        seen_at_height: BlockHeight,
        /// Shielded pool the note was on.
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
