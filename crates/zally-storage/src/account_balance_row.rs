//! Per-pool account balance row returned by
//! [`WalletStorage::get_account_balance`](crate::WalletStorage::get_account_balance).
//!
//! Storage exposes typed [`Zatoshis`] amounts directly, not raw `u64`. The wallet layer
//! attaches the [`Network`] tag from its own handle when projecting to its public
//! `AccountBalance` view.
//!
//! [`Network`]: zally_core::Network

use zally_core::{BlockHeight, Zatoshis};

/// One per-pool balance snapshot for a single account, anchored to the wallet's last
/// observed chain tip.
///
/// Sapling and Orchard values report the upstream spendable totals reported by
/// `WalletRead::get_wallet_summary` (notes whose witnesses are computable and whose
/// confirmation depth is met).
///
/// Transparent values split by ZIP-213 coinbase maturity: coinbase outputs need 100
/// confirmations before they become spendable; everything else is spendable on the first
/// confirmation. The maturity check uses `as_of_height + 1` as the target height, matching
/// `zcash_client_backend`'s `chain_tip + 1` convention so the split agrees with
/// `WalletRead::get_wallet_summary().unshielded_balance().spendable_value()` for the mature
/// half. Outputs already consumed by a confirmed or still-unconfirmed wallet-owned spend
/// are excluded from both fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct AccountBalanceRow {
    /// Spendable Sapling value.
    pub sapling_zat: Zatoshis,
    /// Spendable Orchard value.
    pub orchard_zat: Zatoshis,
    /// Transparent value past the ZIP-213 coinbase maturity gate at `as_of_height`.
    pub transparent_mature_zat: Zatoshis,
    /// Transparent value still inside the 100-block coinbase maturity window.
    pub transparent_immature_zat: Zatoshis,
    /// Chain tip the row was computed against, or `None` when the wallet has not yet
    /// recorded a tip.
    pub as_of_height: Option<BlockHeight>,
}
