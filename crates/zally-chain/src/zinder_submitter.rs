//! Live [`Submitter`] implementation backed by `zinder_client::ChainIndex::broadcast_transaction`.

use std::sync::Arc;

use async_trait::async_trait;
use zally_core::{Network, TxId};
use zinder_client::ChainIndex;
use zinder_core::{RawTransactionBytes, TransactionBroadcastResult};

use crate::error::SubmitterError;
use crate::submitter::{RejectionReason, SubmitOutcome, Submitter};

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

/// Sentinel for outcome variants the upstream node does not echo a tx id on.
///
/// `Duplicate` and `Queued` both fall into this bucket: zinder forwards the
/// upstream Zebra reply, which omits the transaction identifier in both
/// cases. Callers that need the actual tx id pass their pre-computed value
/// as the fallback at the next layer up (`spend.rs::resolve_send_outcome`).
const ECHO_TX_ID_PLACEHOLDER: [u8; 32] = [0_u8; 32];

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
            tx_id: TxId::from_bytes(ECHO_TX_ID_PLACEHOLDER),
        },
        TransactionBroadcastResult::Queued(_queued) => SubmitOutcome::Queued {
            tx_id: TxId::from_bytes(ECHO_TX_ID_PLACEHOLDER),
        },
        TransactionBroadcastResult::InvalidEncoding(invalid) => SubmitOutcome::Rejected {
            reason: RejectionReason::Unknown,
            detail: format!("invalid encoding: {}", invalid.message),
        },
        TransactionBroadcastResult::Rejected(rejected) => SubmitOutcome::Rejected {
            reason: rejected.kind,
            detail: rejected.message,
        },
        TransactionBroadcastResult::Unknown(unknown) => SubmitOutcome::Rejected {
            reason: RejectionReason::Unknown,
            detail: format!("unknown outcome: {}", unknown.message),
        },
        _ => SubmitOutcome::Rejected {
            reason: RejectionReason::Unknown,
            detail: "zinder returned an unrecognised broadcast outcome variant".into(),
        },
    }
}
