//! BIP-44 transparent gap-limit policy.
//!
//! The gap limit is the maximum number of consecutive unmined external transparent
//! address reservations a wallet will hold open before refusing new ones. The policy
//! is a wallet-wide invariant: the same numbers must be honored by the storage
//! backend that pre-derives addresses, by the PCZT signer that matches transparent
//! inputs against derived keys, and by anything else that walks the BIP-44 window.
//! Centralizing the values here keeps the limits from drifting across crates.

/// Per-scope gap-limit budgets, in BIP-44's "consecutive unused indices" units.
///
/// The three scopes match `zcash_keys::keys::transparent::gap_limits::GapLimits`:
///
/// - `external`: receiving addresses surfaced to a payer.
/// - `internal`: change addresses created during proposal construction.
/// - `ephemeral`: ZIP-320 TEX single-use addresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentGapLimit {
    /// Externally-visible receiving addresses.
    pub external: u32,
    /// Internal change addresses.
    pub internal: u32,
    /// Ephemeral ZIP-320 TEX addresses.
    pub ephemeral: u32,
}

impl TransparentGapLimit {
    /// Zally's wallet-policy gap-limit defaults.
    ///
    /// The values sit above `zcash_client_sqlite`'s upstream defaults
    /// (`external: 10`, `internal: 5`, `ephemeral: 10`). The upstream's
    /// pre-generation window interacts probabilistically with Sapling diversifier
    /// validity, so a 10-wide external window can reject the first reservation
    /// on a randomly-seeded fresh wallet whenever the first ten diversifier
    /// indices happen to lack a valid Sapling candidate. Raising the window to
    /// 20 drops the per-seed failure probability from ~0.1% to ~10^-6, and also
    /// gives an operator a more comfortable payment-request budget between
    /// confirmations.
    pub const DEFAULT: Self = Self {
        external: 20,
        internal: 10,
        ephemeral: 20,
    };
}

impl Default for TransparentGapLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}
