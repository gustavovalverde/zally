//! Operator-facing view of a shielded receive, spent or unspent.

use zally_chain::ShieldedPool;
use zally_core::{BlockHeight, TxId, Zatoshis};

/// One Sapling or Orchard note ever observed for an account.
///
/// Returned by [`crate::Wallet::list_shielded_receives`]. Distinguishes itself from
/// [`crate::UnspentShieldedNote`] by including notes that have already been spent: the
/// record reports the receive event itself, not the current spendability state. Operators
/// use this to rebuild downstream observation tables (donation indices, custody timelines)
/// from chain truth at boot or after a wipe, classifying each row identically to the
/// matching `WalletEvent::ShieldedReceiveObserved` from the live stream.
///
/// `is_change` and `spent_our_inputs` together let a consumer attribute each receive:
/// a self-funded change output sets both true; a transparent or shielded sweep of the
/// wallet's own funds sets `spent_our_inputs` true without `is_change`; a third-party
/// transfer leaves both false.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct ShieldedReceiveRecord {
    /// Pool this note lives on.
    pub pool: ShieldedPool,
    /// Note value in zatoshis.
    pub value: Zatoshis,
    /// Transaction that produced this note.
    pub tx_id: TxId,
    /// Output index within the producing transaction (Sapling output index or Orchard
    /// action index, depending on `pool`).
    pub output_index: u32,
    /// Block height at which the producing transaction was mined.
    pub mined_height: BlockHeight,
    /// Block header timestamp in milliseconds (Unix epoch), sourced from the
    /// `blocks.time` column at `mined_height`. Zero when the wallet's local blocks
    /// table does not retain the row (e.g., truncated by `truncate_to_height`).
    pub block_timestamp_ms: u64,
    /// `zcash_client_sqlite` marked this note as change for the receiving account.
    pub is_change: bool,
    /// The producing transaction spent at least one input owned by the receiving account,
    /// across Sapling, Orchard, or transparent pools.
    pub spent_our_inputs: bool,
}
