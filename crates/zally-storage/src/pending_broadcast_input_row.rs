//! Pending broadcast input row returned by
//! [`WalletStorage::list_pending_broadcast_inputs`](crate::WalletStorage::list_pending_broadcast_inputs).

use zally_core::{BlockHeight, OutPoint, TxId, Zatoshis};

/// A transparent outpoint currently locked by a wallet-owned transaction that has been
/// broadcast but not yet observed mined.
///
/// Rows live in the Zally-owned `ext_zally_pending_broadcast_inputs` table. Sync clears
/// rows whose spending transaction is observed mined; rows older than the open-time inflight
/// window get cleared whether or not the transaction is observed.
///
/// Field names match the public wallet view ([`crate::PendingTransparentInput`](crate::PendingBroadcastInputRow))
/// so no rename happens across the storage/wallet boundary: `broadcast_tx_id` is the tx
/// that locked the outpoint by spending it (the wallet-owned broadcast), and the wallet
/// view exposes the same identifier under the same name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PendingBroadcastInputRow {
    /// Identifier of the wallet-owned broadcast that consumed the outpoint.
    pub broadcast_tx_id: TxId,
    /// The transparent outpoint locked by the broadcast.
    pub outpoint: OutPoint,
    /// Value of the locked outpoint.
    pub value_zat: Zatoshis,
    /// Unix milliseconds when the wallet recorded the broadcast.
    pub broadcast_at_ms: u64,
    /// Chain tip the wallet had observed at broadcast time. `None` when the wallet had not
    /// yet recorded a tip.
    pub broadcast_at_height: Option<BlockHeight>,
}
