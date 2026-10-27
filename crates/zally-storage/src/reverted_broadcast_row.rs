//! Reverted broadcast row returned by
//! [`WalletStorage::list_reverted_broadcasts`](crate::WalletStorage::list_reverted_broadcasts).

use zally_core::{BlockHeight, TxId};

/// A wallet-sent transaction whose chain record the wallet lost before its fate was settled.
///
/// Rows live in the Zally-owned `ext_zally_reverted_broadcasts` table. A rewind writes one,
/// in the same database transaction, for each wallet-sent transaction mined in a block it
/// removed. A rebuild from the birthday writes one for each wallet-created transaction that
/// was unmined, still named by an idempotency key, or still had pending-broadcast rows. The
/// row keeps the raw transaction and its expiry so the wallet can resubmit it, notice it mined
/// again, or judge it unmineable, even though the rebuild discarded the transaction records
/// `zcash_client_sqlite` keeps.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RevertedBroadcastRow {
    /// Identifier of the reverted transaction.
    pub tx_id: TxId,
    /// Serialized transaction, as the wallet stored it when it was created.
    pub raw_tx_bytes: Vec<u8>,
    /// Last height at which the transaction can be mined. Height `0` means it never expires.
    pub expiry_height: BlockHeight,
    /// Height the transaction was mined at before the wallet lost its record, or `None` when
    /// it was unmined.
    pub reverted_from_height: Option<BlockHeight>,
    /// Height the wallet has since scanned the transaction mined at, if any.
    pub mined_height: Option<BlockHeight>,
    /// Unix milliseconds of the last resubmission, or `None` before the first.
    pub resubmitted_at_ms: Option<u64>,
}
