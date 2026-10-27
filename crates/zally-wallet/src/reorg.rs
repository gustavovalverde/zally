//! Rewinds after a reorg and reconciliation of the payment ledger they disturb.
//!
//! A reorg the chain source announces carries its fork point: the block below the first
//! reverted height. [`Wallet::rewind_to_fork_point`] rewinds the wallet to exactly that block
//! at any depth, unless the wallet already follows the replacement chain, and records the
//! announcing event's cursor in the same storage transaction. Every rewind, announced or
//! found by the repair ladder, goes through [`Wallet::revert_to_height`], which reports what
//! left the chain as [`WalletEvent`]s and leaves the wallet-sent transactions it un-mined in
//! the reverted-broadcast ledger. [`Wallet::reconcile_payment_ledger`] then submits each one
//! again while it can be mined, retires it once it is mined again, and retires any sent
//! transaction that can no longer be mined, releasing its idempotency key.

use zally_chain::{ChainEpoch, ChainEventCursor, ChainSource, Submitter};
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

    /// Settles the payment ledger against the chain the wallet just caught up with.
    ///
    /// A wallet-sent transaction that can no longer be mined is retired first: it was never
    /// mined at or below its expiry height, and both the settled tip and the wallet's fully
    /// scanned height have passed that height, so no reorg can bring it back. Its
    /// idempotency keys are released for a replacement payment and the wallet publishes
    /// [`WalletEvent::TransactionExpired`]; its inputs are spendable again.
    ///
    /// Every other transaction a rewind removed from the chain is retired once the wallet
    /// has scanned it mined again (the scan already published
    /// [`WalletEvent::TransactionConfirmed`]), or submitted once through `submitter` while
    /// the next block can still include it. Without a submitter the wallet waits for the
    /// chain to mine it or for it to expire.
    ///
    /// Callers run this against a freshly recorded chain tip.
    pub(crate) async fn reconcile_payment_ledger(
        &self,
        submitter: Option<&dyn Submitter>,
    ) -> Result<(), WalletError> {
        let chain_tips = self.inner.storage.find_chain_tips().await?;
        let fully_scanned = self.inner.storage.fully_scanned_height().await?;
        if let (Some(chain_tips), Some(fully_scanned)) = (chain_tips, fully_scanned) {
            let final_height = chain_tips.settled.min(fully_scanned);
            for (tx_id, expiry_height) in self
                .inner
                .storage
                .retire_expired_submissions(final_height, current_unix_ms())
                .await?
            {
                tracing::warn!(
                    target: "zally::sync",
                    event = "wallet_submission_expired",
                    tx_id = %tx_id,
                    expiry_height = expiry_height.as_u32(),
                    "sent transaction can no longer be mined; its idempotency key is released"
                );
                self.publish_event(WalletEvent::TransactionExpired {
                    tx_id,
                    expiry_height,
                });
            }
        }

        let next_height = chain_tips.map_or(0, |tips| tips.visible.as_u32().saturating_add(1));
        for reverted in self.inner.storage.list_reverted_broadcasts().await? {
            if reverted.mined_height.is_some() {
                self.inner
                    .storage
                    .clear_reverted_broadcast(reverted.tx_id)
                    .await?;
                continue;
            }
            let expiry_height = reverted.expiry_height.as_u32();
            let is_minable = expiry_height == 0 || next_height <= expiry_height;
            let Some(submitter) = submitter else {
                continue;
            };
            if reverted.resubmitted_at_ms.is_some() || !is_minable {
                continue;
            }
            self.resubmit_reverted_broadcast(submitter, reverted.tx_id, &reverted.raw_tx_bytes)
                .await?;
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
