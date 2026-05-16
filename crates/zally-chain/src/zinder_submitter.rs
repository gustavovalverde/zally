//! Live [`Submitter`] implementation backed by `zinder_client::ChainIndex::broadcast_transaction`.

use std::sync::Arc;

use async_trait::async_trait;
use zally_core::{Network, TxId};
use zinder_client::ChainIndex;
use zinder_core::{RawTransactionBytes, TransactionBroadcastResult};

use crate::chain_error::SubmitterError;
use crate::submitter::{SubmitOutcome, Submitter};

/// Live `Submitter` backed by a [`zinder_client::ChainIndex`].
///
/// `ZinderSubmitter` is `Clone`; cloning shares the underlying gRPC channel via `Arc`.
#[derive(Clone)]
pub struct ZinderSubmitter {
    inner: Arc<dyn ChainIndex>,
    network: Network,
}

impl std::fmt::Debug for ZinderSubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZinderSubmitter")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl ZinderSubmitter {
    /// Wraps an already-constructed [`ChainIndex`] as a submitter for `network`.
    #[must_use]
    pub fn from_chain_index(inner: Arc<dyn ChainIndex>, network: Network) -> Self {
        Self { inner, network }
    }
}

#[async_trait]
impl Submitter for ZinderSubmitter {
    fn network(&self) -> Network {
        self.network
    }

    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        let bytes = RawTransactionBytes::new(raw_tx.to_vec());
        let outcome = self.inner.broadcast_transaction(bytes).await?;
        Ok(translate_broadcast_outcome(outcome))
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive zinder broadcast outcomes map unknown variants to Rejected"
)]
fn translate_broadcast_outcome(outcome: TransactionBroadcastResult) -> SubmitOutcome {
    match outcome {
        TransactionBroadcastResult::Accepted(accepted) => SubmitOutcome::Accepted {
            tx_id: TxId::from_bytes(accepted.transaction_id.as_bytes()),
        },
        TransactionBroadcastResult::Duplicate(_duplicate) => SubmitOutcome::Duplicate {
            // zinder's BroadcastDuplicate does not echo the transaction_id back. The
            // Duplicate variant signals idempotency-preserving acceptance.
            tx_id: TxId::from_bytes([0_u8; 32]),
        },
        TransactionBroadcastResult::InvalidEncoding(invalid) => SubmitOutcome::Rejected {
            reason: format!("invalid encoding: {}", invalid.message),
        },
        TransactionBroadcastResult::Rejected(rejected) => SubmitOutcome::Rejected {
            reason: format!(
                "rejected ({}): {}",
                rejected
                    .error_code
                    .map_or_else(|| "no-code".into(), |c| c.to_string()),
                rejected.message
            ),
        },
        TransactionBroadcastResult::Unknown(unknown) => SubmitOutcome::Rejected {
            reason: format!("unknown outcome: {}", unknown.message),
        },
        _ => SubmitOutcome::Rejected {
            reason: "zinder returned an unrecognised broadcast outcome variant".into(),
        },
    }
}
