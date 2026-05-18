//! Operator-facing view of a previously-exposed Unified Address.

use zally_core::{BlockHeight, Network};
use zcash_keys::address::UnifiedAddress;
use zip32::DiversifierIndex;

/// A Unified Address previously exposed for one wallet account.
///
/// Returned by [`crate::Wallet::list_exposed_addresses`] in derivation order. Operators use
/// this view to pair a deployment-time configured receiver (e.g. a configured miner address)
/// with the diversifier the wallet recorded, without re-deriving and without burning a
/// transparent gap-limit slot.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ExposedAddress {
    /// Network the wallet is bound to.
    pub network: Network,
    /// The Unified Address itself.
    pub unified_address: UnifiedAddress,
    /// The diversifier index that produced this address.
    pub diversifier_index: DiversifierIndex,
    /// Whether this UA carries a P2PKH (transparent) receiver. False for the shielded-only
    /// UAs returned by [`crate::Wallet::derive_next_address`].
    pub has_transparent_receiver: bool,
    /// Block height the wallet first observed the address as exposed, when recorded by the
    /// storage backend. `None` for addresses derived offline or imported without an exposure
    /// height.
    pub exposed_at_height: Option<BlockHeight>,
}
