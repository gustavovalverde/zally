//! Refusal of spends while the chain source follows shallower reorgs than the node produces.
//!
//! The wallet judges a pending broadcast unmineable at its settled height and then lets the
//! broadcast's idempotency key build a replacement. That verdict holds only while the chain
//! source never replaces a block that deep, which requires its reorg window to cover every
//! reorg the node can produce (1,000 blocks for Zebra). Against a shallower window, a reorg
//! the node legitimately produces could still mine the released broadcast after its
//! replacement was paid. The sync driver reads the source's window when it starts and each
//! time it reopens the chain-event stream, and the wallet refuses new spends while the window
//! is below [`crate::SyncDriverOptions::minimum_source_reorg_window_blocks`].

use crate::event::WalletEvent;
use crate::wallet::Wallet;

/// A chain source reorg window below the minimum the sync driver requires.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReorgWindowShortfall {
    pub(crate) reorg_window_blocks: u32,
    pub(crate) required_blocks: u32,
}

impl Wallet {
    /// Records the reorg window the chain source reported against `required_blocks`.
    ///
    /// Refuses new spends while the window is too shallow, and publishes
    /// [`WalletEvent::SourceReorgWindowTooShallow`] when the refusal starts or the reported
    /// window changes, and [`WalletEvent::SourceReorgWindowSufficient`] when it lifts.
    pub(crate) fn record_source_reorg_window(
        &self,
        reorg_window_blocks: u32,
        required_blocks: u32,
    ) {
        let shortfall = (reorg_window_blocks < required_blocks).then_some(ReorgWindowShortfall {
            reorg_window_blocks,
            required_blocks,
        });
        let prior = std::mem::replace(
            &mut *self.inner.source_reorg_window_shortfall.lock(),
            shortfall,
        );
        if prior == shortfall {
            return;
        }
        if shortfall.is_some() {
            tracing::error!(
                target: "zally::wallet",
                event = "source_reorg_window_too_shallow",
                reorg_window_blocks,
                required_blocks,
                "chain source follows shallower reorgs than the node produces; new spends are refused"
            );
            self.publish_event(WalletEvent::SourceReorgWindowTooShallow {
                reorg_window_blocks,
                required_blocks,
            });
        } else if prior.is_some() {
            tracing::info!(
                target: "zally::wallet",
                event = "source_reorg_window_sufficient",
                reorg_window_blocks,
                required_blocks,
                "chain source reorg window meets the minimum again; spends resume"
            );
            self.publish_event(WalletEvent::SourceReorgWindowSufficient {
                reorg_window_blocks,
                required_blocks,
            });
        }
    }

    /// Lifts a spend refusal over the source's reorg window without judging the source, when
    /// no sync driver checks it any longer.
    pub(crate) fn clear_source_reorg_window(&self) {
        *self.inner.source_reorg_window_shortfall.lock() = None;
    }
}
