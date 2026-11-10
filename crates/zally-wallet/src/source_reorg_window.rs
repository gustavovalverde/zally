//! Refusal of spends, and of unmineable verdicts, while the chain source's reorg window is
//! shallower than the node's reorgs or not yet known.
//!
//! The wallet judges a pending broadcast unmineable at its settled height and then lets the
//! broadcast's idempotency key build a replacement. That verdict holds only while the chain
//! source never replaces a block that deep, which requires its reorg window to cover every
//! reorg the node can produce (1,000 blocks for Zebra). Against a shallower window, a reorg
//! the node legitimately produces could still mine the released broadcast after its
//! replacement was paid. With [`crate::WalletOptions::minimum_source_reorg_window_blocks`]
//! set, the wallet reads the source's window right before a verdict would free anything and
//! judges nothing unmineable unless that read meets the minimum. A sync driver also reads it
//! when it starts, each time it reopens the chain-event stream, and on every polling wakeup
//! until a read succeeds, so new spends are refused while the window is too shallow or not yet
//! read.

use std::time::Duration;

use zally_chain::ChainSource;

use crate::event::WalletEvent;
use crate::wallet::Wallet;

/// What the wallet last established about the chain source's reorg window, against
/// [`crate::WalletOptions::minimum_source_reorg_window_blocks`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SourceReorgWindowCheck {
    /// Nothing has read the window, or the sync driver that read it closed.
    #[default]
    Unchecked,
    /// A sync driver requires the minimum and has not yet read the window.
    Unverified,
    /// The window the source reported is below the minimum.
    TooShallow { reorg_window_blocks: u32 },
    /// The window the source reported meets the minimum.
    Sufficient,
}

impl Wallet {
    /// Marks the window unverified when a sync driver starts under a minimum, before its task
    /// runs, until its first successful read.
    pub(crate) fn start_source_reorg_window_check(&self) {
        if self
            .inner
            .options
            .minimum_source_reorg_window_blocks
            .is_some()
        {
            *self.inner.source_reorg_window.lock() = SourceReorgWindowCheck::Unverified;
        }
    }

    /// Reads the source's reorg window, waiting at most `timeout`, and records it; returns
    /// whether the window meets the minimum, so a verdict may free anything. Without a minimum
    /// it always does. A read that fails or times out does not, and keeps the prior record.
    pub(crate) async fn verify_source_reorg_window(
        &self,
        chain: &dyn ChainSource,
        timeout: Duration,
    ) -> bool {
        let Some(required_blocks) = self.inner.options.minimum_source_reorg_window_blocks else {
            return true;
        };
        match tokio::time::timeout(timeout, chain.reorg_window_blocks()).await {
            Ok(Ok(reorg_window_blocks)) => {
                self.record_source_reorg_window(reorg_window_blocks, required_blocks);
                reorg_window_blocks >= required_blocks
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    target: "zally::wallet",
                    event = "wallet_source_reorg_window_unreadable",
                    %error,
                    "could not read the chain source reorg window; keeping the prior result, \
                     and unmineable verdicts wait"
                );
                false
            }
            Err(_elapsed) => {
                tracing::warn!(
                    target: "zally::wallet",
                    event = "wallet_source_reorg_window_unreadable",
                    timeout_ms = timeout.as_millis(),
                    "chain source reorg window read timed out; keeping the prior result, and \
                     unmineable verdicts wait"
                );
                false
            }
        }
    }

    /// Records the reorg window the chain source reported against `required_blocks`.
    ///
    /// Publishes [`WalletEvent::SourceReorgWindowTooShallow`] when a shortfall starts or the
    /// reported window changes, and [`WalletEvent::SourceReorgWindowSufficient`] when a
    /// shortfall lifts.
    fn record_source_reorg_window(&self, reorg_window_blocks: u32, required_blocks: u32) {
        let check = if reorg_window_blocks < required_blocks {
            SourceReorgWindowCheck::TooShallow {
                reorg_window_blocks,
            }
        } else {
            SourceReorgWindowCheck::Sufficient
        };
        let prior = std::mem::replace(&mut *self.inner.source_reorg_window.lock(), check);
        match (prior, check) {
            (prior, SourceReorgWindowCheck::TooShallow { .. }) if prior != check => {
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
            (SourceReorgWindowCheck::TooShallow { .. }, SourceReorgWindowCheck::Sufficient) => {
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

    /// Forgets the window check when the sync driver that kept it closes.
    pub(crate) fn clear_source_reorg_window(&self) {
        *self.inner.source_reorg_window.lock() = SourceReorgWindowCheck::Unchecked;
    }

    /// Whether the last read showed a window below the minimum, so a sync reads it again
    /// even without a verdict to back, and the refusal lifts once the source meets it.
    pub(crate) fn is_source_reorg_window_too_shallow(&self) -> bool {
        matches!(
            *self.inner.source_reorg_window.lock(),
            SourceReorgWindowCheck::TooShallow { .. }
        )
    }

    /// Whether a sync driver requires a window it has not read yet.
    pub(crate) fn is_source_reorg_window_unverified(&self) -> bool {
        *self.inner.source_reorg_window.lock() == SourceReorgWindowCheck::Unverified
    }
}
