//! Refusal of PCZTs that can never be mined.
//!
//! A PCZT created before a network upgrade activates commits to the old consensus branch, so
//! every block after activation rejects its transaction, however often it is signed again
//! (ZIP 259). The wallet checks a PCZT at every role entry point and releases the inputs held
//! for one that can never be mined.

use zally_chain::TransactionHeader;
use zally_pczt::{PcztBytes, PcztError};
use zally_storage::StorageError;

use crate::error::WalletError;
use crate::wallet::Wallet;

impl Wallet {
    /// Rejects a PCZT that can never be mined, after releasing the inputs held for it.
    ///
    /// The PCZT is judged at the wallet's recorded visible tip. A wallet that has recorded no
    /// tip cannot judge it and leaves the verdict to the network at submission. The verdict
    /// assumes this wallet is the PCZT's only broadcaster: a copy extracted and broadcast
    /// elsewhere may already be mined, and recreating its payment would pay twice.
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
