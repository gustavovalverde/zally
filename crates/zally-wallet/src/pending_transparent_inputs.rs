//! Operator-facing snapshot of transparent outpoints locked by wallet-owned broadcasts.

use zally_core::{AccountId, BlockHeight, Network, OutPoint, TxId, Zatoshis};

/// One transparent outpoint locked by a wallet-owned transaction that has been broadcast
/// but not yet observed mined.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct PendingTransparentInput {
    /// The locked outpoint.
    pub outpoint: OutPoint,
    /// Value of the locked outpoint.
    pub value_zat: Zatoshis,
    /// Identifier of the broadcast that locked the outpoint.
    pub broadcast_tx_id: TxId,
    /// Unix milliseconds when the wallet recorded the broadcast.
    pub broadcast_at_ms: u64,
    /// Chain tip the wallet had observed at broadcast time. `None` when no tip was recorded.
    pub broadcast_at_height: Option<BlockHeight>,
}

/// Snapshot of every transparent outpoint currently locked by wallet-owned broadcasts for
/// one account, anchored to the wallet's last observed chain tip.
///
/// Returned by [`crate::Wallet::get_pending_transparent_inputs`]. Operators use this view to
/// reason about "what funds are in flight right now?" without inspecting the spending
/// transactions directly.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct PendingTransparentInputs {
    /// Network the wallet is bound to.
    pub network: Network,
    /// Account that owns the broadcasts.
    pub account_id: AccountId,
    /// The locked outpoints, ordered by broadcast time and spending txid.
    pub inputs: Vec<PendingTransparentInput>,
    /// Chain tip the snapshot is anchored to, or `None` when the wallet has not yet
    /// recorded a tip.
    pub as_of_height: Option<BlockHeight>,
}
