//! Reverted broadcast row returned by
//! [`WalletStorage::list_reverted_broadcasts`](crate::WalletStorage::list_reverted_broadcasts).

use zally_core::{BlockHeight, TxId};

/// A wallet-sent transaction that was mined in a block a rewind later removed from the chain.
///
/// Rows live in the Zally-owned `ext_zally_reverted_broadcasts` table, written in the same
/// database transaction as the rewind. The row keeps the raw transaction and its expiry so the
/// sync driver can resubmit it, notice it mined again, or declare it expired, even across a
/// wallet rebuild that discards the transaction records `zcash_client_sqlite` keeps.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RevertedBroadcastRow {
    /// Identifier of the reverted transaction.
    pub tx_id: TxId,
    /// Serialized transaction, as the wallet stored it when it was created.
    pub raw_tx_bytes: Vec<u8>,
    /// Last height at which the transaction can be mined. Height `0` means it never expires.
    pub expiry_height: BlockHeight,
    /// Height the transaction was mined at before the rewind.
    pub reverted_from_height: BlockHeight,
    /// Height the wallet has since scanned the transaction mined at, if any.
    pub mined_height: Option<BlockHeight>,
    /// Unix milliseconds of the last resubmission, or `None` before the first.
    pub resubmitted_at_ms: Option<u64>,
}
