//! Retirement of pending broadcasts once the chain settles their fate.
//!
//! A pending broadcast stays recorded until it is mined at or below the judged height, or
//! until no block above that height can include it. Wall-clock age never retires one: a
//! settled tip can trail the visible tip by the chain source's whole reorg window.

use zally_chain::{ChainSource, TransactionStatus, UnmineableReason, parse_transaction_header};
use zally_core::{BlockHeight, TxId};
use zally_storage::UnmineableHold;

use crate::error::WalletError;
use crate::event::WalletEvent;
use crate::wallet::Wallet;

impl Wallet {
    /// Retires every pending broadcast whose fate the chain settled.
    ///
    /// Broadcasts are judged at the lower of the fully scanned height and the settled tip. The
    /// wallet has seen every block up to that height, and a reorg within the chain source's
    /// bound cannot replace them. A broadcast mined at or below it drops its pending inputs. A
    /// broadcast not mined there that no later block accepts is recorded as unmineable once,
    /// which drops its pending inputs, frees its idempotency key for a replacement, and
    /// reopens the dispense holds finalized with it; [`WalletEvent::PendingBroadcastUnmineable`]
    /// reports it and [`WalletEvent::HoldDispenseUnmineable`] reports each hold. A
    /// transaction a rewind removed from the chain is judged the same way. Judging at the visible tip
    /// instead could report a broadcast that an unscanned or reorged block still mines, and
    /// its replacement would pay twice. No broadcast is recorded unmineable while the sync
    /// driver holds verdicts over the chain source's reorg window, because a settled tip the
    /// source can still replace cannot back one.
    ///
    /// The verdict also asks `chain` before it frees anything, because the wallet's scan can
    /// miss a transaction it sent: it records the broadcast unmineable only when the source
    /// reports the transaction not found. One the source reports mined is recorded as mined at
    /// that height, with the earlier steps of its send, and keeps its key; one the source
    /// still holds, or a failed lookup, leaves it unjudged until a later sync.
    pub(crate) async fn retire_pending_broadcasts(
        &self,
        chain: &dyn ChainSource,
    ) -> Result<(), WalletError> {
        let storage = &self.inner.storage;
        let (Some(scanned_height), Some(settled_tip_height)) = (
            storage.fully_scanned_height().await?,
            storage.find_settled_tip().await?,
        ) else {
            return Ok(());
        };
        let judged_height = scanned_height.min(settled_tip_height);
        let is_verdict_held = self.is_source_reorg_window_holding();
        for pending in storage.list_pending_broadcast_transactions().await? {
            let tx_id = pending.broadcast_tx_id;
            match pending.mined_height {
                Some(mined_height) if mined_height <= judged_height => {
                    storage
                        .clear_pending_broadcast_inputs_for_mined(&[tx_id])
                        .await?;
                    continue;
                }
                Some(_) => continue,
                None if is_verdict_held => continue,
                None => {}
            }
            let header = match parse_transaction_header(&pending.raw_bytes) {
                Ok(header) => header,
                Err(error) => {
                    tracing::error!(
                        target: "zally::wallet",
                        event = "pending_broadcast_unreadable",
                        %tx_id,
                        %error,
                        "stored pending broadcast could not be parsed"
                    );
                    continue;
                }
            };
            let Err(reason) = header.check_mineable(self.network(), judged_height) else {
                continue;
            };
            if !self
                .is_absent_from_chain(chain, tx_id, &pending.raw_bytes)
                .await
            {
                continue;
            }
            if let Some(holds) = storage
                .record_unmineable_transaction(tx_id, judged_height)
                .await?
            {
                self.report_unmineable(tx_id, &reason, judged_height, holds);
            }
        }
        Ok(())
    }

    /// Reports a broadcast just recorded unmineable and each dispense hold it settled.
    fn report_unmineable(
        &self,
        tx_id: TxId,
        reason: &UnmineableReason,
        judged_height: BlockHeight,
        holds: Vec<UnmineableHold>,
    ) {
        tracing::warn!(
            target: "zally::wallet",
            event = "pending_broadcast_unmineable",
            %tx_id,
            %reason,
            judged_height = judged_height.as_u32(),
            "pending broadcast can never be mined; recreate it"
        );
        self.publish_event(WalletEvent::PendingBroadcastUnmineable {
            tx_id,
            reason: reason.to_string(),
        });
        for hold in holds {
            tracing::warn!(
                target: "zally::wallet",
                event = "hold_dispense_unmineable",
                hold_id = %hold.hold_id.as_uuid(),
                %tx_id,
                is_reopened = hold.is_reopened,
                "dispense hold was finalized with a transaction that can never be mined"
            );
            self.publish_event(WalletEvent::HoldDispenseUnmineable {
                hold_id: hold.hold_id,
                request_id: hold.request_id,
                tx_id,
                is_reopened: hold.is_reopened,
            });
        }
    }

    /// Whether `chain` reports `tx_id` absent, the only answer that lets the verdict record it
    /// unmineable.
    ///
    /// A transaction the source reports mined is recorded mined, so the wallet stops judging it
    /// and its key keeps answering with it.
    async fn is_absent_from_chain(
        &self,
        chain: &dyn ChainSource,
        tx_id: TxId,
        raw_bytes: &[u8],
    ) -> bool {
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "a status the wallet does not know cannot prove the transaction absent"
        )]
        match chain.transaction_status(tx_id).await {
            Ok(TransactionStatus::NotFound) => true,
            Ok(TransactionStatus::Confirmed {
                confirmed_at_height,
                ..
            }) => {
                tracing::warn!(
                    target: "zally::wallet",
                    event = "pending_broadcast_mined_unrecognized",
                    %tx_id,
                    confirmed_at_height = confirmed_at_height.as_u32(),
                    "chain source reports a pending broadcast mined that the wallet's scan did \
                     not recognize; recording it mined"
                );
                self.record_send_confirmed(chain, tx_id, raw_bytes, confirmed_at_height)
                    .await;
                false
            }
            Ok(status) => {
                tracing::info!(
                    target: "zally::wallet",
                    event = "pending_broadcast_still_known",
                    %tx_id,
                    ?status,
                    "chain source still knows a pending broadcast the wallet judged unmineable; \
                     leaving it unjudged"
                );
                false
            }
            Err(error) => {
                tracing::warn!(
                    target: "zally::wallet",
                    event = "pending_broadcast_status_unavailable",
                    %tx_id,
                    %error,
                    "could not confirm a pending broadcast is absent from the chain; leaving it \
                     unjudged"
                );
                false
            }
        }
    }

    /// Records `tx_id` mined at `confirmed_at_height`, after the earlier steps of its send that
    /// the source also reports mined, so their outputs are known before the step that spends
    /// them. A failure is logged and retried by a later sync, since the broadcast stays pending.
    async fn record_send_confirmed(
        &self,
        chain: &dyn ChainSource,
        tx_id: TxId,
        raw_bytes: &[u8],
        confirmed_at_height: BlockHeight,
    ) {
        let storage = &self.inner.storage;
        let earlier_steps = match storage.list_send_steps(tx_id).await {
            Ok(steps) => steps,
            Err(error) => {
                tracing::warn!(
                    target: "zally::wallet",
                    event = "pending_broadcast_steps_unreadable",
                    %tx_id,
                    %error,
                    "could not read the steps of a send the chain source reports mined"
                );
                Vec::new()
            }
        };
        let mut confirmed = Vec::new();
        for step in earlier_steps
            .into_iter()
            .filter(|step| step.tx_id != tx_id && step.mined_height.is_none())
        {
            let Some(step_bytes) = step.raw_bytes else {
                continue;
            };
            if let Ok(TransactionStatus::Confirmed {
                confirmed_at_height: step_height,
                ..
            }) = chain.transaction_status(step.tx_id).await
            {
                confirmed.push((step_bytes, step_height));
            }
        }
        confirmed.push((raw_bytes.to_vec(), confirmed_at_height));
        for (step_bytes, step_height) in confirmed {
            if let Err(error) = storage
                .record_confirmed_broadcast(step_bytes, step_height)
                .await
            {
                tracing::warn!(
                    target: "zally::wallet",
                    event = "pending_broadcast_confirmation_record_failed",
                    %tx_id,
                    %error,
                    "could not record a send the chain source reports mined; a later sync retries"
                );
                return;
            }
        }
    }
}
