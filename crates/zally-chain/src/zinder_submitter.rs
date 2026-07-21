//! Live [`Submitter`] implementation backed by `zinder_client::EndpointBackedIndex::broadcast_transaction`.

use std::sync::Arc;

use async_trait::async_trait;
use zally_core::{Network, TxId};
use zinder_client::EndpointBackedIndex;
use zinder_client::{WALLET_BROADCAST_TRANSACTION_V1, WALLET_READ_SERVER_INFO_V2};
use zinder_core::{RawTransactionBytes, TransactionBroadcastOutcome};

use crate::error::SubmitterError;
use crate::submitter::{RejectionReason, SubmitOutcome, Submitter};
use crate::transaction::parse_transaction_id;
use crate::zinder_source::ZinderCapabilitySet;

/// Live `Submitter` backed by a [`zinder_client::EndpointBackedIndex`].
///
/// `ZinderSubmitter` is `Clone`; cloning shares the underlying gRPC channel via `Arc`.
#[derive(Clone)]
pub struct ZinderSubmitter {
    inner: Arc<dyn EndpointBackedIndex>,
    network: Network,
    capabilities: Arc<tokio::sync::OnceCell<ZinderCapabilitySet>>,
}

impl std::fmt::Debug for ZinderSubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZinderSubmitter")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl ZinderSubmitter {
    /// Wraps an already-constructed [`EndpointBackedIndex`] as a submitter for `network`.
    #[must_use]
    pub fn from_chain_index(inner: Arc<dyn EndpointBackedIndex>, network: Network) -> Self {
        Self {
            inner,
            network,
            capabilities: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    pub(crate) fn from_chain_index_with_capabilities(
        inner: Arc<dyn EndpointBackedIndex>,
        network: Network,
        capabilities: Arc<tokio::sync::OnceCell<ZinderCapabilitySet>>,
    ) -> Self {
        Self {
            inner,
            network,
            capabilities,
        }
    }
}

#[async_trait]
impl Submitter for ZinderSubmitter {
    fn network(&self) -> Network {
        self.network
    }

    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        let capabilities = self
            .capabilities
            .get_or_try_init(|| async {
                let descriptor = self.inner.server_info().await?;
                let common = descriptor.common.ok_or_else(|| {
                    zinder_client::IndexerError::MalformedResponse {
                        field: "info.common",
                        reason: "field is missing".to_owned(),
                    }
                })?;
                if common.contract_revision < 2 {
                    return Err(zinder_client::IndexerError::FailedPrecondition {
                        reason: format!(
                            "wallet contract revision {} is older than required revision 2",
                            common.contract_revision
                        ),
                    });
                }
                Ok::<_, zinder_client::IndexerError>(common.capabilities.into_iter().collect())
            })
            .await?;
        let required = [WALLET_READ_SERVER_INFO_V2, WALLET_BROADCAST_TRANSACTION_V1];
        let missing_capabilities = required
            .into_iter()
            .filter(|capability| !capabilities.contains(*capability))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !missing_capabilities.is_empty() {
            return Err(SubmitterError::CapabilitiesUnavailable {
                capabilities: missing_capabilities,
            });
        }
        let bytes = RawTransactionBytes::new(raw_tx.to_vec());
        let outcome = self.inner.broadcast_transaction(bytes).await?;
        translate_broadcast_outcome(outcome, parse_transaction_id(raw_tx))
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive zinder broadcast outcomes fail closed on unknown variants"
)]
fn translate_broadcast_outcome(
    outcome: TransactionBroadcastOutcome,
    submitted_tx_id: Result<TxId, crate::transaction::TransactionParseError>,
) -> Result<SubmitOutcome, SubmitterError> {
    match outcome {
        TransactionBroadcastOutcome::Accepted(accepted) => {
            let submitted_tx_id =
                submitted_tx_id.map_err(|_| SubmitterError::UnsupportedResponse {
                    response: "accepted broadcast named a transaction for malformed submitted bytes",
                })?;
            let accepted_tx_id = TxId::from_bytes(accepted.transaction_id.as_bytes());
            if accepted_tx_id != submitted_tx_id {
                return Err(SubmitterError::UnsupportedResponse {
                    response: "accepted broadcast transaction id did not match submitted bytes",
                });
            }
            Ok(SubmitOutcome::Accepted {
                tx_id: submitted_tx_id,
            })
        }
        TransactionBroadcastOutcome::Duplicate(_duplicate) => Ok(SubmitOutcome::Duplicate {
            tx_id: submitted_tx_id.map_err(|_| SubmitterError::UnsupportedResponse {
                response: "duplicate broadcast omitted the transaction id for malformed submitted bytes",
            })?,
        }),
        TransactionBroadcastOutcome::Queued(_queued) => Ok(SubmitOutcome::Queued {
            tx_id: submitted_tx_id.map_err(|_| SubmitterError::UnsupportedResponse {
                response: "queued broadcast omitted the transaction id for malformed submitted bytes",
            })?,
        }),
        TransactionBroadcastOutcome::InvalidEncoding(invalid) => Ok(SubmitOutcome::Rejected {
            reason: RejectionReason::InvalidEncoding,
            detail: invalid.message,
        }),
        TransactionBroadcastOutcome::Rejected(rejected) => Ok(SubmitOutcome::Rejected {
            reason: rejected.kind.into(),
            detail: rejected.message,
        }),
        TransactionBroadcastOutcome::Unknown(unknown) => Ok(SubmitOutcome::Rejected {
            reason: RejectionReason::Unknown,
            detail: format!("unknown outcome: {}", unknown.message),
        }),
        _ => Err(SubmitterError::UnsupportedResponse {
            response: "TransactionBroadcastOutcome",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::translate_broadcast_outcome;
    use crate::{RejectionReason, SubmitOutcome};
    use zally_core::TxId;
    use zinder_core::{
        BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastQueued,
        TransactionBroadcastOutcome, TransactionId as ZinderTransactionId,
    };

    #[test]
    fn duplicate_uses_the_submitted_transaction_id() -> Result<(), crate::SubmitterError> {
        let submitted_tx_id = TxId::from_bytes([1; 32]);
        let outcome = translate_broadcast_outcome(
            TransactionBroadcastOutcome::Duplicate(BroadcastDuplicate {
                error_code: None,
                message: "already known".to_owned(),
            }),
            Ok(submitted_tx_id),
        )?;

        assert_eq!(
            outcome,
            SubmitOutcome::Duplicate {
                tx_id: submitted_tx_id,
            }
        );
        Ok(())
    }

    #[test]
    fn accepted_rejects_a_transaction_id_other_than_the_submitted_transaction() {
        let outcome = translate_broadcast_outcome(
            TransactionBroadcastOutcome::Accepted(BroadcastAccepted {
                transaction_id: ZinderTransactionId::from_bytes([2; 32]),
            }),
            Ok(TxId::from_bytes([1; 32])),
        );

        assert!(matches!(
            outcome,
            Err(crate::SubmitterError::UnsupportedResponse {
                response: "accepted broadcast transaction id did not match submitted bytes",
            })
        ));
    }

    #[test]
    fn queued_uses_the_submitted_transaction_id() -> Result<(), crate::SubmitterError> {
        let submitted_tx_id = TxId::from_bytes([2; 32]);
        let outcome = translate_broadcast_outcome(
            TransactionBroadcastOutcome::Queued(BroadcastQueued {
                message: "queued".to_owned(),
            }),
            Ok(submitted_tx_id),
        )?;

        assert_eq!(
            outcome,
            SubmitOutcome::Queued {
                tx_id: submitted_tx_id,
            }
        );
        Ok(())
    }

    #[test]
    fn invalid_encoding_has_a_typed_rejection_reason() -> Result<(), crate::SubmitterError> {
        let outcome = translate_broadcast_outcome(
            TransactionBroadcastOutcome::InvalidEncoding(BroadcastInvalidEncoding {
                error_code: None,
                message: "truncated".to_owned(),
            }),
            Err(crate::TransactionParseError::Read {
                reason: "truncated fixture".to_owned(),
            }),
        )?;

        assert_eq!(
            outcome,
            SubmitOutcome::Rejected {
                reason: RejectionReason::InvalidEncoding,
                detail: "truncated".to_owned(),
            }
        );
        Ok(())
    }
}
