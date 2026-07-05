//! Operator-facing per-pool account balance snapshot.

use zally_core::{BlockHeight, Network, Zatoshis};

/// Per-pool balance for one wallet account, anchored to the wallet's last observed chain
/// tip.
///
/// Returned by [`crate::Wallet::get_account_balance`]. Operators use this view as the
/// canonical funding-gate read for a deployment that needs to know "what can I pay out
/// right now?" without summing individual notes in the consumer.
///
/// Shielded pool fields (`sapling_zat`, `orchard_zat`, `ironwood_zat`) report the spendable
/// value reported by the underlying wallet summary: notes whose witnesses are computable and
/// whose confirmation depth is met.
///
/// Transparent fields split by ZIP-213 coinbase maturity computed against `as_of_height`:
/// `transparent_mature_zat` is the value `shield_transparent_funds` is allowed to consume
/// at the snapshot tip; `transparent_immature_zat` is the value still inside the
/// 100-block coinbase maturity window. Outputs already consumed by a confirmed wallet-owned
/// spend are excluded from both fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct AccountBalance {
    /// Network the wallet is bound to.
    pub network: Network,
    /// Spendable Sapling value.
    pub sapling_zat: Zatoshis,
    /// Spendable Orchard value.
    pub orchard_zat: Zatoshis,
    /// Spendable Ironwood value.
    pub ironwood_zat: Zatoshis,
    /// Transparent value past the ZIP-213 coinbase maturity gate at `as_of_height`.
    pub transparent_mature_zat: Zatoshis,
    /// Transparent value still inside the 100-block coinbase maturity window.
    pub transparent_immature_zat: Zatoshis,
    /// Chain tip the snapshot is anchored to, or `None` when the wallet has not yet
    /// recorded a tip.
    pub as_of_height: Option<BlockHeight>,
}

impl AccountBalance {
    /// Returns the spending-pool view: Sapling plus Orchard plus Ironwood spendable.
    #[must_use]
    pub const fn shielded_zat(&self) -> Zatoshis {
        self.sapling_zat
            .saturating_add(self.orchard_zat)
            .saturating_add(self.ironwood_zat)
    }

    /// Returns the reporting view across the transparent pool: mature plus immature.
    #[must_use]
    pub const fn transparent_zat(&self) -> Zatoshis {
        self.transparent_mature_zat
            .saturating_add(self.transparent_immature_zat)
    }

    /// Returns the sum of every pool tracked by this snapshot.
    #[must_use]
    pub const fn total_zat(&self) -> Zatoshis {
        self.shielded_zat().saturating_add(self.transparent_zat())
    }
}
