//! Previously-exposed Unified Address row returned by
//! [`WalletStorage::list_exposed_addresses`](crate::WalletStorage::list_exposed_addresses).

use zally_core::BlockHeight;
use zcash_keys::address::UnifiedAddress;
use zip32::DiversifierIndex;

/// One Unified Address previously exposed for the account, with the metadata needed to
/// pair it to a configured receiver (e.g. miner address) without re-deriving.
///
/// Rows are returned in derivation order (ascending by exposure height, then by diversifier
/// index). Reading the list never advances a diversifier index and never burns a transparent
/// gap-limit slot, so it is safe to call on every diagnostics poll.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ExposedAddressRow {
    /// The Unified Address itself.
    pub unified_address: UnifiedAddress,
    /// The diversifier index that produced this address.
    pub diversifier_index: DiversifierIndex,
    /// Whether this UA carries a P2PKH (transparent) receiver. False for the shielded-only
    /// UAs returned by `derive_next_address`.
    pub has_transparent_receiver: bool,
    /// Block height the storage backend first recorded the address as exposed. `None` for
    /// addresses derived offline or imported without an exposure height.
    pub exposed_at_height: Option<BlockHeight>,
}
