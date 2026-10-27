//! Refusal of transactions that can never be mined.
//!
//! A transaction signed before a network upgrade activates commits to the old consensus
//! branch, so every block after activation rejects it, however often it is signed again
//! (ZIP 259). The wallet checks pending broadcasts on every sync and PCZTs at every role entry
//! point, and releases what it holds for a transaction that can never be mined.

use zally_chain::{TransactionHeader, parse_transaction_header};
use zally_pczt::{PcztBytes, PcztError};
use zally_storage::StorageError;

use crate::error::WalletError;
use crate::event::WalletEvent;
use crate::wallet::Wallet;

impl Wallet {
    /// Releases the recorded inputs of every pending broadcast that can never be mined and
    /// publishes [`WalletEvent::PendingBroadcastUnmineable`] for it.
    ///
    /// A broadcast is judged at the lower of the fully scanned height and the settled tip. The
    /// wallet has seen every block up to that height, and a reorg within the chain source's
    /// bound cannot replace them, so a broadcast not mined there that no later block accepts
    /// is never mined. Judging at the visible tip instead could report a broadcast that an
    /// unscanned or reorged block still mines, and its recreation would pay twice.
    pub(crate) async fn retire_unmineable_pending_broadcasts(&self) -> Result<(), WalletError> {
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
            storage
                .clear_pending_broadcast_inputs_for_mined(&[tx_id])
                .await?;
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
        }
        Ok(())
    }

    /// Rejects a PCZT that can never be mined, after releasing the inputs held for it.
    ///
    /// The PCZT is judged at the wallet's recorded visible tip. A wallet that has recorded no
    /// tip cannot judge it and leaves the verdict to the network at submission.
    pub(crate) async fn reject_unmineable_pczt(&self, pczt: &PcztBytes) -> Result<(), WalletError> {
        let Some(visible_tip_height) = self.inner.storage.find_visible_tip().await? else {
            return Ok(());
        };
        let parsed = pczt.parse()?;
        let global = parsed.global();
        let header = TransactionHeader::from_pczt_global(
            *global.tx_version(),
            *global.version_group_id(),
            *global.consensus_branch_id(),
            *global.expiry_height(),
        )
        .map_err(|error| PcztError::ParseFailed {
            reason: error.to_string(),
        })?;
        let Err(reason) = header.check_mineable(self.network(), visible_tip_height) else {
            return Ok(());
        };
        let unmineable = WalletError::TransactionUnmineable { reason };
        match self.abandon_pczt(pczt).await {
            // A PCZT created outside this wallet holds none of its inputs.
            Ok(()) | Err(WalletError::Storage(StorageError::PcztLockOwnerMissing)) => {
                Err(unmineable)
            }
            Err(cleanup_error) => {
                tracing::error!(
                    target: "zally::wallet",
                    event = "unmineable_pczt_cleanup_failed",
                    %unmineable,
                    %cleanup_error,
                    "PCZT can never be mined and its exact input locks could not be released"
                );
                Err(cleanup_error)
            }
        }
    }
}
