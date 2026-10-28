//! Open-time configuration for a [`crate::Wallet`].

use crate::circuit_breaker::CircuitBreakerConfig;

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
    /// Smallest chain source reorg window, in blocks, under which the wallet still judges
    /// pending broadcasts unmineable and accepts new spends. `None` disables the check.
    ///
    /// The wallet frees an idempotency key once it judges the key's transaction unmineable at
    /// the source's settled tip, so the promise never to pay twice holds only while the source
    /// follows every reorg the node can produce. [`crate::Wallet::sync`] and
    /// [`crate::Wallet::scan_queued_range`] read [`zally_chain::ChainSource::reorg_window_blocks`]
    /// right before a verdict would record a pending broadcast unmineable, and record nothing
    /// unless the read meets the minimum. A [`crate::SyncDriver`] also reads it when it starts,
    /// each time it reopens the chain-event stream, and on every polling wakeup until a read
    /// succeeds. While the last read showed a smaller window, or a driver has not read it yet,
    /// new spends fail with [`crate::WalletError::SourceReorgWindowTooShallow`] or
    /// [`crate::WalletError::SourceReorgWindowUnverified`].
    ///
    /// Default: 1,000 blocks, the deepest reorg Zebra produces.
    pub minimum_source_reorg_window_blocks: Option<u32>,
    /// Failure threshold and cooldown of the circuit breaker that guards the wallet's chain
    /// reads and submissions.
    ///
    /// Default: [`CircuitBreakerConfig::default`], five consecutive retryable failures and a
    /// one-minute cooldown.
    pub circuit_breaker: CircuitBreakerConfig,
}

impl WalletOptions {
    /// Default age after which pending broadcast inputs are flagged stale: 2 hours.
    pub const DEFAULT_PENDING_BROADCAST_WINDOW_MS: u64 = 2 * 60 * 60 * 1000;

    /// Default minimum chain source reorg window: 1,000 blocks. Zebra commits a block to
    /// finalized state only once its best chain is longer than this, so it never presents a
    /// deeper reorg.
    pub const DEFAULT_MINIMUM_SOURCE_REORG_WINDOW_BLOCKS: u32 = 1_000;

    /// Returns a copy of `self` with `circuit_breaker` replaced.
    #[must_use]
    pub const fn with_circuit_breaker_config(
        mut self,
        circuit_breaker: CircuitBreakerConfig,
    ) -> Self {
        self.circuit_breaker = circuit_breaker;
        self
    }

    /// Returns a copy of `self` with `minimum_source_reorg_window_blocks` replaced.
    #[must_use]
    pub const fn with_minimum_source_reorg_window_blocks(
        mut self,
        minimum_source_reorg_window_blocks: Option<u32>,
    ) -> Self {
        self.minimum_source_reorg_window_blocks = minimum_source_reorg_window_blocks;
        self
    }

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
            minimum_source_reorg_window_blocks: Some(
                Self::DEFAULT_MINIMUM_SOURCE_REORG_WINDOW_BLOCKS,
            ),
            circuit_breaker: CircuitBreakerConfig::default(),
        }
    }
}
