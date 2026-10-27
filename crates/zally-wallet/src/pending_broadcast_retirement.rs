//! Retirement of pending broadcasts once the chain settles their fate.
//!
//! A pending broadcast stays recorded until it is mined at or below the judged height, or
//! until no block above that height can include it. Wall-clock age never retires one: a
//! settled tip can trail the visible tip by the chain source's whole reorg window.

use zally_chain::parse_transaction_header;

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
    /// its replacement would pay twice.
    pub(crate) async fn retire_pending_broadcasts(&self) -> Result<(), WalletError> {
        let storage = &self.inner.storage;
        let (Some(scanned_height), Some(settled_tip_height)) = (
            storage.fully_scanned_height().await?,
            storage.find_settled_tip().await?,
        ) else {
            return Ok(());
        };
        let judged_height = scanned_height.min(settled_tip_height);
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
            let Some(holds) = storage
                .record_unmineable_transaction(tx_id, judged_height)
                .await?
            else {
                continue;
            };
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
        Ok(())
    }
}
