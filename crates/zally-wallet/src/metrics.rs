//! Operator-readable wallet metrics snapshot.

use zally_core::{BlockHeight, Network};

use crate::wallet::Wallet;
use crate::wallet_error::WalletError;

/// Typed snapshot of the wallet's observable state.
///
/// Operators wire this into Prometheus / OpenTelemetry / their own metrics adapter; Zally
/// does not bake in a metrics backend.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct WalletMetrics {
    /// Network this wallet is bound to.
    pub network: Network,
    /// Highest block height the wallet has scanned, if any. `None` until the first
    /// successful [`Wallet::sync`] call records progress (Slice 5 follow-up).
    pub scanned_height: Option<BlockHeight>,
    /// Chain tip the wallet's most recent sync observed, if any.
    pub chain_tip_height: Option<BlockHeight>,
    /// Number of accounts the wallet manages. v1 fixes this at 1.
    pub account_count: u32,
    /// Number of subscribers currently attached to [`Wallet::observe`].
    pub event_subscriber_count: u32,
}

impl Wallet {
    /// Returns a typed metrics snapshot.
    ///
    /// `not_retryable` only for catastrophic storage failures; otherwise infallible. Slice 5
    /// keeps `scanned_height` and `chain_tip_height` as `None` because live scanning is on
    /// the v1 follow-up list; the surface is stable.
    #[allow(
        clippy::unused_async,
        reason = "async surface is the v1 contract; later slices fill the body with awaited \
                  storage and chain-tip lookups"
    )]
    pub async fn metrics_snapshot(&self) -> Result<WalletMetrics, WalletError> {
        Ok(WalletMetrics {
            network: self.network(),
            scanned_height: None,
            chain_tip_height: None,
            account_count: 1,
            event_subscriber_count: self.observer_count(),
        })
    }
}
