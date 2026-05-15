//! Operator-readable wallet metrics snapshot.

use zally_core::{BlockHeight, Network};

use crate::circuit_breaker::CircuitBreakerState;
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
    /// successful [`Wallet::sync`] call records progress.
    pub scanned_height: Option<BlockHeight>,
    /// Chain tip the wallet's most recent sync observed, if any.
    pub chain_tip_height: Option<BlockHeight>,
    /// Number of blocks between `scanned_height` and `chain_tip_height`, if both are known
    /// and the tip has not regressed.
    pub lag_blocks: Option<u32>,
    /// Number of accounts the wallet manages. Always 1: Zally holds one account per wallet.
    pub account_count: u32,
    /// Number of subscribers currently attached to [`Wallet::observe`].
    pub event_subscriber_count: u32,
    /// Current outbound IO circuit-breaker state.
    pub circuit_breaker: CircuitBreakerState,
}

impl Wallet {
    /// Returns a typed metrics snapshot.
    ///
    /// `not_retryable` only for catastrophic storage failures; otherwise infallible.
    /// `scanned_height` and `chain_tip_height` stay `None` until the first successful
    /// [`Wallet::sync`] records progress.
    pub async fn metrics_snapshot(&self) -> Result<WalletMetrics, WalletError> {
        let status = self.status_snapshot().await?;
        Ok(WalletMetrics {
            network: status.network,
            scanned_height: status.scanned_height,
            chain_tip_height: status.chain_tip_height,
            lag_blocks: status.lag_blocks,
            account_count: status.account_count,
            event_subscriber_count: status.event_subscriber_count,
            circuit_breaker: status.circuit_breaker,
        })
    }
}
