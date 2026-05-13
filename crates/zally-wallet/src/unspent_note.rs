//! Operator-facing view of an unspent shielded note.

use zally_chain::ShieldedPool;
use zally_core::{BlockHeight, TxId, Zatoshis};

/// An unspent Sapling or Orchard note owned by a wallet account.
///
/// Returned by [`crate::Wallet::list_unspent_shielded_notes`]. Operators use this view as a
/// snapshot of spendable shielded inputs for an account: balance dashboards, observation
/// channels (e.g., donation observers), and reservation logic before a custody flow.
///
/// The `confirmations` field is computed against the wallet's last observed chain tip at
/// the moment of the call. Operators that need a fresher number should call
/// [`crate::Wallet::sync`] first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct UnspentShieldedNote {
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
    /// Confirmations on this note computed as `observed_tip - mined_height + 1`, saturating
    /// at `0` if the wallet has not yet observed a tip at or above `mined_height`.
    pub confirmations: u32,
}
