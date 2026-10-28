//! Open-time configuration for a [`crate::Wallet`].

/// Wallet open-time configuration.
///
/// Constructed at `Wallet::create` / `Wallet::open` / `Wallet::open_or_create_account` and
/// stored on the resulting handle. Options stay private to the wallet instance; nothing is
/// persisted to storage. A fresh `WalletOptions::default()` is always safe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct WalletOptions {
    /// Age after which `Wallet::get_pending_transparent_inputs` flags a pending broadcast's
    /// inputs as stale.
    ///
    /// The flag is an operator signal only. A pending broadcast stays recorded until
    /// `Wallet::sync` sees it mined at a settled height or judges it unmineable, however old
    /// it is, because the settled tip can trail the chain by the source's whole reorg window.
    /// Whether an input can be selected again is librustzcash's decision, from its locks,
    /// persisted spends, and transaction expiry.
    ///
    /// Default: 2 hours (7,200,000 ms).
    pub pending_broadcast_window_ms: u64,
}

impl WalletOptions {
    /// Default age after which pending broadcast inputs are flagged stale: 2 hours.
    pub const DEFAULT_PENDING_BROADCAST_WINDOW_MS: u64 = 2 * 60 * 60 * 1000;

    /// Returns a copy of `self` with `pending_broadcast_window_ms` set to `window_ms`.
    #[must_use]
    pub const fn with_pending_broadcast_window_ms(mut self, window_ms: u64) -> Self {
        self.pending_broadcast_window_ms = window_ms;
        self
    }
}

impl Default for WalletOptions {
    fn default() -> Self {
        Self {
            pending_broadcast_window_ms: Self::DEFAULT_PENDING_BROADCAST_WINDOW_MS,
        }
    }
}
