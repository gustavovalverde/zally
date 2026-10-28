//! Refusal of spends, and of unmineable verdicts, while the chain source's reorg window is
//! shallower than the node's reorgs or not yet known.
//!
//! The wallet judges a pending broadcast unmineable at its settled height and then lets the
//! broadcast's idempotency key build a replacement. That verdict holds only while the chain
//! source never replaces a block that deep, which requires its reorg window to cover every
//! reorg the node can produce (1,000 blocks for Zebra). Against a shallower window, a reorg
//! the node legitimately produces could still mine the released broadcast after its
//! replacement was paid. A sync driver with
//! [`crate::SyncDriverOptions::minimum_source_reorg_window_blocks`] reads the source's window
//! when it starts and each time it reopens the chain-event stream. Until a read meets the
//! minimum, the wallet refuses new spends and records no broadcast as unmineable, so no key is
//! freed under a window that cannot back the verdict.

use crate::event::WalletEvent;
use crate::wallet::Wallet;

/// What the sync driver last established about the chain source's reorg window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SourceReorgWindowCheck {
    /// No sync driver checks the window.
    #[default]
    Unchecked,
    /// A sync driver requires `required_blocks` and has not yet read the window.
    Unverified { required_blocks: u32 },
    /// The window the source reported is below the minimum the sync driver requires.
    TooShallow {
        reorg_window_blocks: u32,
        required_blocks: u32,
    },
    /// The window the source reported meets the minimum.
    Sufficient,
}

impl SourceReorgWindowCheck {
    /// Whether the wallet must hold new spends and unmineable verdicts.
    pub(crate) const fn is_holding(self) -> bool {
        matches!(self, Self::Unverified { .. } | Self::TooShallow { .. })
    }
}

impl Wallet {
    /// Marks the window unverified when a sync driver that requires `required_blocks` starts,
    /// until its first read.
    pub(crate) fn start_source_reorg_window_check(&self, required_blocks: u32) {
        *self.inner.source_reorg_window.lock() =
            SourceReorgWindowCheck::Unverified { required_blocks };
    }

    /// Records the reorg window the chain source reported against `required_blocks`.
    ///
    /// Publishes [`WalletEvent::SourceReorgWindowTooShallow`] when a shortfall starts or the
    /// reported window changes, and [`WalletEvent::SourceReorgWindowSufficient`] when a
    /// shortfall lifts.
    pub(crate) fn record_source_reorg_window(
        &self,
        reorg_window_blocks: u32,
        required_blocks: u32,
    ) {
        let check = if reorg_window_blocks < required_blocks {
            SourceReorgWindowCheck::TooShallow {
                reorg_window_blocks,
                required_blocks,
            }
        } else {
            SourceReorgWindowCheck::Sufficient
        };
        let prior = std::mem::replace(&mut *self.inner.source_reorg_window.lock(), check);
        if prior == check {
            return;
        }
        match (prior, check) {
            (_, SourceReorgWindowCheck::TooShallow { .. }) => {
                tracing::error!(
                    target: "zally::wallet",
                    event = "source_reorg_window_too_shallow",
                    reorg_window_blocks,
                    required_blocks,
                    "chain source follows shallower reorgs than the node produces; new spends \
                     and unmineable verdicts are held"
                );
                self.publish_event(WalletEvent::SourceReorgWindowTooShallow {
                    reorg_window_blocks,
                    required_blocks,
                });
            }
            (SourceReorgWindowCheck::TooShallow { .. }, _) => {
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
            _ => {}
        }
    }

    /// Forgets the window check when no sync driver checks it any longer.
    pub(crate) fn clear_source_reorg_window(&self) {
        *self.inner.source_reorg_window.lock() = SourceReorgWindowCheck::Unchecked;
    }

    /// Whether the window check holds unmineable verdicts.
    pub(crate) fn is_source_reorg_window_holding(&self) -> bool {
        self.inner.source_reorg_window.lock().is_holding()
    }
}
