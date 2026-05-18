//! Open-time configuration for a [`crate::Wallet`].

/// Wallet open-time configuration.
///
/// Constructed at `Wallet::create` / `Wallet::open` / `Wallet::open_or_create_account` and
/// stored on the resulting handle. Options stay private to the wallet instance; nothing is
/// persisted to storage. A fresh `WalletOptions::default()` is always safe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct WalletOptions {
    /// How long after a successful broadcast a wallet-owned transaction's transparent inputs
    /// stay excluded from new input selection. The exclusion is what stops the auto-shield
    /// loop from selecting the same outpoints across consecutive ticks before the spending
    /// tx has been observed mined.
    ///
    /// The same value drives `Wallet::get_pending_transparent_inputs`: it bounds how far
    /// back the snapshot reaches. Entries older than the window are dropped from the
    /// snapshot and become eligible inputs again on the next spend.
    ///
    /// Default: 2 hours (7,200,000 ms). Operators on faster networks (mainnet 75-second
    /// blocks) may reduce; testnet/regtest deployments can stay at the default.
    pub pending_broadcast_window_ms: u64,
}

impl WalletOptions {
    /// Default inflight window for pending wallet-owned broadcasts: 2 hours.
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
