//! Operator-readable wallet status snapshot.

use zally_core::{BlockHeight, Network};

use crate::circuit_breaker::CircuitBreakerState;
use crate::wallet::Wallet;
use crate::wallet_error::WalletError;

/// Wallet scan state derived from persisted wallet progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SyncStatus {
    /// The wallet has not observed a chain tip and has no recorded scan progress.
    NotStarted,
    /// The wallet has scan progress, but no observed chain tip is recorded.
    WaitingForTip {
        /// Highest block height the wallet has scanned.
        scanned_height: BlockHeight,
    },
    /// The wallet has observed a chain tip and has not recorded scan progress yet.
    Starting {
        /// Chain tip the wallet is catching up to.
        target_height: BlockHeight,
    },
    /// The wallet is behind the most recently observed chain tip.
    CatchingUp {
        /// Highest block height the wallet has scanned.
        scanned_height: BlockHeight,
        /// Chain tip the wallet is catching up to.
        target_height: BlockHeight,
        /// Number of blocks between `scanned_height` and `target_height`.
        lag_blocks: u32,
    },
    /// The wallet has scanned to the most recently observed chain tip.
    AtTip {
        /// Tip height the wallet has scanned to.
        tip_height: BlockHeight,
    },
    /// The last observed chain tip is lower than the wallet's scanned height.
    TipRegressed {
        /// Highest block height the wallet has scanned.
        scanned_height: BlockHeight,
        /// Lower chain tip most recently observed.
        chain_tip_height: BlockHeight,
    },
}

/// Typed snapshot of wallet state for readiness and operations.
///
/// Operators should expose this to health checks and logs. Metrics adapters can derive
/// counters and gauges from it without coupling to storage internals.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct WalletStatus {
    /// Network this wallet is bound to.
    pub network: Network,
    /// Scan status derived from persisted wallet progress.
    pub sync_status: SyncStatus,
    /// Highest block height the wallet has scanned, if any.
    pub scanned_height: Option<BlockHeight>,
    /// Chain tip the wallet most recently observed, if any.
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
    /// Returns a typed wallet status snapshot.
    ///
    /// `retryable` on transient storage I/O. `requires_operator` on storage integrity
    /// failures surfaced by the storage implementation.
    pub async fn status_snapshot(&self) -> Result<WalletStatus, WalletError> {
        let scanned_height = self.inner.storage.fully_scanned_height().await?;
        let chain_tip_height = self.inner.storage.lookup_observed_tip().await?;
        let sync_status = sync_status_from_heights(scanned_height, chain_tip_height);
        Ok(WalletStatus {
            network: self.network(),
            sync_status,
            scanned_height,
            chain_tip_height,
            lag_blocks: compute_lag_blocks(scanned_height, chain_tip_height),
            account_count: 1,
            event_subscriber_count: self.observer_count(),
            circuit_breaker: self.circuit_breaker_state(),
        })
    }
}

fn sync_status_from_heights(
    scanned_height: Option<BlockHeight>,
    chain_tip_height: Option<BlockHeight>,
) -> SyncStatus {
    match (scanned_height, chain_tip_height) {
        (None, None) => SyncStatus::NotStarted,
        (Some(scanned), None) => SyncStatus::WaitingForTip {
            scanned_height: scanned,
        },
        (None, Some(tip)) => SyncStatus::Starting { target_height: tip },
        (Some(scanned), Some(tip)) => match scanned.as_u32().cmp(&tip.as_u32()) {
            std::cmp::Ordering::Less => SyncStatus::CatchingUp {
                scanned_height: scanned,
                target_height: tip,
                lag_blocks: tip.as_u32() - scanned.as_u32(),
            },
            std::cmp::Ordering::Equal => SyncStatus::AtTip { tip_height: tip },
            std::cmp::Ordering::Greater => SyncStatus::TipRegressed {
                scanned_height: scanned,
                chain_tip_height: tip,
            },
        },
    }
}

fn compute_lag_blocks(
    scanned_height: Option<BlockHeight>,
    chain_tip_height: Option<BlockHeight>,
) -> Option<u32> {
    let scanned = scanned_height?;
    let tip = chain_tip_height?;
    if tip.as_u32() < scanned.as_u32() {
        None
    } else {
        Some(tip.as_u32() - scanned.as_u32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_status_reports_not_started_without_heights() {
        assert_eq!(sync_status_from_heights(None, None), SyncStatus::NotStarted);
        assert_eq!(compute_lag_blocks(None, None), None);
    }

    #[test]
    fn sync_status_reports_lag_when_tip_is_ahead() {
        assert_eq!(
            sync_status_from_heights(Some(BlockHeight::from(7)), Some(BlockHeight::from(10))),
            SyncStatus::CatchingUp {
                scanned_height: BlockHeight::from(7),
                target_height: BlockHeight::from(10),
                lag_blocks: 3,
            }
        );
        assert_eq!(
            compute_lag_blocks(Some(BlockHeight::from(7)), Some(BlockHeight::from(10))),
            Some(3)
        );
    }

    #[test]
    fn sync_status_reports_tip_regress_without_negative_lag() {
        assert_eq!(
            sync_status_from_heights(Some(BlockHeight::from(10)), Some(BlockHeight::from(7))),
            SyncStatus::TipRegressed {
                scanned_height: BlockHeight::from(10),
                chain_tip_height: BlockHeight::from(7),
            }
        );
        assert_eq!(
            compute_lag_blocks(Some(BlockHeight::from(10)), Some(BlockHeight::from(7))),
            None
        );
    }
}
