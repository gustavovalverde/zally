//! Send step row returned by
//! [`WalletStorage::list_send_steps`](crate::WalletStorage::list_send_steps).

use zally_core::{BlockHeight, TxId};

/// One transaction of a send an idempotency key is bound to, in submission order.
///
/// A single-step send has one step. A multi-step (ZIP 320) send lists the steps that fund the
/// paying transaction first and the paying transaction last.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SendStepRow {
    /// Identifier of the step's transaction.
    pub tx_id: TxId,
    /// Serialized transaction, when the wallet holds it, from `zcash_client_sqlite` or the
    /// reverted-broadcast ledger.
    pub raw_bytes: Option<Vec<u8>>,
    /// Height `zcash_client_sqlite` has seen the transaction mined at, if any.
    pub mined_height: Option<BlockHeight>,
}
