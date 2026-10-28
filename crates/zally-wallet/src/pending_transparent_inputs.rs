//! Operator-facing snapshot of transparent outpoints locked by wallet-owned broadcasts.

use zally_core::{AccountId, BlockHeight, Network, OutPoint, TxId, Zatoshis};

/// One transparent outpoint spent by a wallet-owned broadcast whose fate the chain has not
/// settled.
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
    /// The broadcast is older than [`crate::WalletOptions::pending_broadcast_window_ms`] and
    /// the chain has neither mined it at a settled height nor ruled it out. Worth an
    /// operator's look; the entry stays until the chain settles it.
    pub is_stale: bool,
}

/// Snapshot of every transparent outpoint spent by wallet-owned broadcasts the chain has not
/// settled, for one account, anchored to the wallet's persisted visible tip.
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
