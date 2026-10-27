//! Rewinds after a reorg and reconciliation of the payment ledger they disturb.
//!
//! A reorg the chain source announces carries its fork point: the block below the first
//! reverted height. [`Wallet::rewind_to_fork_point`] rewinds the wallet to exactly that block
//! at any depth, unless the wallet already follows the replacement chain, and records the
//! announcing event's cursor in the same storage transaction. Every rewind, announced or
//! found by the repair ladder, goes through [`Wallet::revert_to_height`], which reports what
//! left the chain as [`WalletEvent`]s and leaves the wallet-sent transactions it un-mined in
//! the reverted-broadcast ledger. Those transactions are pending broadcasts again: the
//! pending-broadcast verdict retires one that can never be mined, and
//! [`Wallet::reconcile_reverted_broadcasts`] submits one again while it can be mined and
//! drops it from the ledger once it is mined again.

use zally_chain::{ChainEpoch, ChainEventCursor, ChainSource, Submitter, parse_transaction_header};
use zally_core::BlockHeight;

use crate::error::WalletError;
use crate::event::WalletEvent;
use crate::sync::chain_state_at;
use crate::wallet::{Wallet, current_unix_ms};

/// What [`Wallet::rewind_to_fork_point`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ForkPointRewind {
    /// The wallet held blocks the reorg replaced and now ends at `to_height`.
    Rewound {
        /// Height the wallet now ends at.
        to_height: BlockHeight,
    },
    /// The wallet held no block the reorg replaced, so only the cursor moved.
    Unaffected,
}

impl Wallet {
    /// Applies an announced reorg whose last common block is `fork_height`, then records
    /// `applied_cursor` as the chain-event cursor.
    ///
    /// Rewinds only when the wallet holds a block above the fork point that the source no
    /// longer serves: the highest block the wallet scanned, capped at the source's visible
    /// tip, must carry the hash the source reports at that height. Replaying an event the
    /// wallet already followed is therefore a no-op, which makes the cursor safe to record
    /// after the rewind rather than with it.
    pub(crate) async fn rewind_to_fork_point(
        &self,
        chain: &dyn ChainSource,
        fork_height: BlockHeight,
        applied_cursor: ChainEventCursor,
    ) -> Result<ForkPointRewind, WalletError> {
        let chain_epoch = self.pin_chain_epoch(chain).await?;
        if self
            .follows_chain_above(chain, chain_epoch, fork_height)
            .await?
        {
            self.inner
                .storage
                .record_chain_event_cursor(applied_cursor.into_bytes())
                .await?;
            return Ok(ForkPointRewind::Unaffected);
        }
        let to_height = self
            .revert_to_height(chain, chain_epoch, fork_height, Some(applied_cursor))
            .await?;
        Ok(ForkPointRewind::Rewound { to_height })
    }

    /// Whether every block the wallet holds above `fork_height` is still on the source's
    /// chain.
    async fn follows_chain_above(
        &self,
        chain: &dyn ChainSource,
        chain_epoch: ChainEpoch,
        fork_height: BlockHeight,
    ) -> Result<bool, WalletError> {
        let Some(highest_scanned) = self.inner.storage.find_highest_scanned_height().await? else {
            return Ok(true);
        };
        if highest_scanned <= fork_height {
            return Ok(true);
        }
        let probe_height = highest_scanned.min(chain_epoch.visible_tip().height);
        if probe_height <= fork_height {
            return Ok(false);
        }
        let held_hash = self
            .inner
            .storage
            .find_scanned_block_hash(probe_height)
            .await?;
        let canonical_hash = chain_state_at(chain, chain_epoch, probe_height)
            .await?
            .block_hash;
        Ok(held_hash == Some(canonical_hash))
    }

    /// Rewinds the wallet to exactly `target`, never below the block before the account
    /// birthday, and reports what left the chain.
    ///
    /// Fetches the source's tree state at the target from `chain_epoch`, so the rewind lands
    /// on the replacement chain's own frontier. Publishes [`WalletEvent::ReorgDetected`],
    /// then one [`WalletEvent::TransactionReverted`] per un-mined wallet transaction and one
    /// [`WalletEvent::ShieldedReceiveReverted`] per un-mined receive. Returns the height the
    /// wallet now ends at.
    pub(crate) async fn revert_to_height(
        &self,
        chain: &dyn ChainSource,
        chain_epoch: ChainEpoch,
        target: BlockHeight,
        applied_cursor: Option<ChainEventCursor>,
    ) -> Result<BlockHeight, WalletError> {
        let birthday = self.inner.storage.account_birthday().await?;
        let target = target.max(BlockHeight::from(birthday.as_u32().saturating_sub(1)));
        let chain_state = chain_state_at(chain, chain_epoch, target).await?;
        let reverted = self
            .inner
            .storage
            .revert_blocks_above(
                chain_state,
                applied_cursor.map(ChainEventCursor::into_bytes),
            )
            .await?;
        tracing::warn!(
            target: "zally::sync",
            event = "wallet_rewound",
            rewound_to_height = target.as_u32(),
            reverted_transaction_count = reverted.transactions.len(),
            reverted_receive_count = reverted.receives.len(),
            "wallet rewound below blocks that left the chain"
        );
        self.publish_event(WalletEvent::ReorgDetected {
            rolled_back_to_height: target,
            new_settled_tip_height: chain_epoch.settled_tip().height,
        });
        for (tx_id, mined_height) in reverted.transactions {
            self.publish_event(WalletEvent::TransactionReverted {
                tx_id,
                mined_height,
            });
        }
        for note in reverted.receives {
            self.publish_event(WalletEvent::ShieldedReceiveReverted {
                account_id: note.account_id,
                tx_id: note.tx_id,
                output_index: note.output_index,
                value_zat: note.value_zat,
                mined_height: note.mined_height,
                pool: crate::sync::shielded_pool_for(note.protocol),
            });
        }
        Ok(target)
    }

    /// Resubmits or retires the wallet-sent transactions a rewind removed from the chain.
    ///
    /// A reverted transaction is a pending broadcast again, so
    /// [`Wallet::retire_pending_broadcasts`] judges it with every other one: once it can never
    /// be mined, that verdict releases its idempotency keys, reopens the dispense holds
    /// finalized with it, and reports it once. This pass handles the rest:
    ///
    /// - one the wallet has scanned mined again leaves the reverted-broadcast ledger; the scan
    ///   already published [`WalletEvent::TransactionConfirmed`];
    /// - any other is submitted once through `submitter` when the next block above the
    ///   visible tip can include it, judged by the same
    ///   [`zally_chain::TransactionHeader::check_mineable`]. Without a submitter the wallet
    ///   waits for the chain to mine it or for the verdict to retire it.
    ///
    /// Callers run this against a freshly recorded chain tip.
    pub(crate) async fn reconcile_reverted_broadcasts(
        &self,
        submitter: Option<&dyn Submitter>,
    ) -> Result<(), WalletError> {
        let storage = &self.inner.storage;
        let reverted_broadcasts = storage.list_reverted_broadcasts().await?;
        if reverted_broadcasts.is_empty() {
            return Ok(());
        }
        let visible_tip = storage.find_visible_tip().await?;
        for reverted in reverted_broadcasts {
            if reverted.mined_height.is_some() {
                storage.clear_reverted_broadcast(reverted.tx_id).await?;
                continue;
            }
            let (Some(submitter), Some(visible_tip), None) =
                (submitter, visible_tip, reverted.resubmitted_at_ms)
            else {
                continue;
            };
            let can_enter_next_block = parse_transaction_header(&reverted.raw_tx_bytes)
                .is_ok_and(|header| header.check_mineable(self.network(), visible_tip).is_ok());
            if can_enter_next_block {
                self.resubmit_reverted_broadcast(submitter, reverted.tx_id, &reverted.raw_tx_bytes)
                    .await?;
            }
        }
        Ok(())
    }

    async fn resubmit_reverted_broadcast(
        &self,
        submitter: &dyn Submitter,
        tx_id: zally_core::TxId,
        raw_tx_bytes: &[u8],
    ) -> Result<(), WalletError> {
        match submitter.submit(raw_tx_bytes).await {
            Ok(outcome) => {
                tracing::info!(
                    target: "zally::sync",
                    event = "wallet_reverted_broadcast_resubmitted",
                    tx_id = %tx_id,
                    outcome = ?outcome,
                    "submitted a reverted transaction again"
                );
                self.inner
                    .storage
                    .record_reverted_broadcast_resubmitted(tx_id, current_unix_ms())
                    .await?;
            }
            Err(error) => tracing::warn!(
                target: "zally::sync",
                event = "wallet_reverted_broadcast_resubmit_failed",
                tx_id = %tx_id,
                %error,
                "could not submit a reverted transaction again; the next reconciliation retries"
            ),
        }
        Ok(())
    }
}
