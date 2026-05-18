//! Transparent transaction outpoint.

use crate::txid::TxId;

/// A reference to a specific output of a transaction.
///
/// Zally-owned alternative to `zcash_transparent::bundle::OutPoint` so the public Zally
/// surface composes only Zally domain types.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OutPoint {
    /// Transaction that produced the output.
    pub tx_id: TxId,
    /// Index of the output within the producing transaction.
    pub output_index: u32,
}

impl OutPoint {
    /// Constructs an outpoint.
    #[must_use]
    pub const fn new(tx_id: TxId, output_index: u32) -> Self {
        Self {
            tx_id,
            output_index,
        }
    }
}
