//! `Submitter` trait for transaction broadcast.

use async_trait::async_trait;
use zally_core::{Network, TxId};

use crate::chain_error::SubmitterError;

/// Outcome of a transaction broadcast.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SubmitOutcome {
    /// The mempool accepted the transaction.
    Accepted {
        /// Resulting transaction identifier.
        tx_id: TxId,
    },
    /// The mempool already had this transaction.
    Duplicate {
        /// Resulting transaction identifier.
        tx_id: TxId,
    },
    /// The transaction was rejected; retrying the same bytes will not succeed.
    Rejected {
        /// Underlying upstream reason description.
        reason: String,
    },
}

/// Transaction broadcast plane.
///
/// Implementations call into a `zinder-client::ChainIndex::broadcast_transaction` or an
/// equivalent submitter. Network mismatch fails closed at construction or at call time.
#[async_trait]
pub trait Submitter: Send + Sync + 'static {
    /// Network this submitter is bound to.
    fn network(&self) -> Network;

    /// Submits `raw_tx`. The outcome discriminates duplicate / rejected / accepted.
    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError>;
}
