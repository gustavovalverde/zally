//! Serialized transaction parsing at the chain-plane boundary.

use zally_core::{BlockHeight, BranchId, FailurePosture, TxId};
use zcash_primitives::transaction::Transaction;

/// Error returned by [`parse_transaction_expiry_height`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransactionParseError {
    /// The supplied bytes were not a parseable Zcash transaction.
    ///
    /// Posture: not retryable. The caller must provide different transaction
    /// bytes before retrying.
    #[error("transaction decode failed: {reason}")]
    Read {
        /// Underlying transaction decoder description.
        reason: String,
    },
}

impl TransactionParseError {
    /// Operator-facing posture describing what the consumer may do.
    #[must_use]
    pub const fn posture(&self) -> FailurePosture {
        match self {
            Self::Read { .. } => FailurePosture::NotRetryable,
        }
    }
}

/// Parses serialized Zcash transaction bytes and returns the committed expiry height.
///
/// The linked librustzcash reader dispatches on the version header carried in
/// the bytes. Zcash v5 and v6 transactions carry the consensus branch id and
/// expiry height in their own header fragments, so the parser does not accept a
/// caller-selected network or version.
///
/// # Errors
///
/// Returns [`TransactionParseError::Read`] when the bytes are truncated,
/// malformed, or use an unsupported transaction format.
pub fn parse_transaction_expiry_height(
    raw_tx_bytes: &[u8],
) -> Result<BlockHeight, TransactionParseError> {
    let transaction = Transaction::read(raw_tx_bytes, BranchId::Nu5).map_err(|err| {
        TransactionParseError::Read {
            reason: err.to_string(),
        }
    })?;
    Ok(transaction.expiry_height().into())
}

/// Parses serialized Zcash transaction bytes and returns their canonical transaction id.
///
/// The linked librustzcash reader applies the version-appropriate transaction-id digest,
/// including ZIP-244 for v5 and later transactions.
///
/// # Errors
///
/// Returns [`TransactionParseError::Read`] when the bytes are truncated,
/// malformed, or use an unsupported transaction format.
pub fn parse_transaction_id(raw_tx_bytes: &[u8]) -> Result<TxId, TransactionParseError> {
    let transaction = Transaction::read(raw_tx_bytes, BranchId::Nu5).map_err(|err| {
        TransactionParseError::Read {
            reason: err.to_string(),
        }
    })?;
    Ok(TxId::from_bytes(*transaction.txid().as_ref()))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "test helpers panic on impossible failures when building minimal transactions"
)]
mod tests {
    use super::{TransactionParseError, parse_transaction_expiry_height, parse_transaction_id};
    use zally_core::{BlockHeight, BranchId};
    use zcash_primitives::transaction::{Transaction, TransactionData, TxVersion};
    use zcash_protocol::consensus::BlockHeight as UpstreamBlockHeight;

    fn build_minimal_v5(expiry_height: u32) -> Vec<u8> {
        let transaction = TransactionData::from_parts(
            TxVersion::V5,
            BranchId::Nu5,
            0,
            UpstreamBlockHeight::from(expiry_height),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .expect("freeze minimal v5 transaction");
        let mut raw_tx_bytes = Vec::new();
        transaction
            .write(&mut raw_tx_bytes)
            .expect("write minimal v5 transaction");
        raw_tx_bytes
    }

    #[test]
    fn transaction_id_matches_the_canonical_transaction_digest() {
        let raw_tx_bytes = build_minimal_v5(123_456);
        let parsed = Transaction::read(raw_tx_bytes.as_slice(), BranchId::Nu5)
            .expect("parse minimal v5 transaction");

        assert_eq!(
            parse_transaction_id(&raw_tx_bytes).expect("derive transaction id"),
            zally_core::TxId::from_bytes(*parsed.txid().as_ref())
        );
    }

    fn build_minimal_v6(expiry_height: u32) -> Vec<u8> {
        let transaction = TransactionData::from_parts_v6(
            BranchId::Nu6_3,
            0,
            UpstreamBlockHeight::from(expiry_height),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .expect("freeze minimal v6 transaction");
        let mut raw_tx_bytes = Vec::new();
        transaction
            .write(&mut raw_tx_bytes)
            .expect("write minimal v6 transaction");
        raw_tx_bytes
    }

    #[test]
    fn parses_v5_expiry_height() {
        let expiry_height = 4_321_098;
        let raw_tx_bytes = build_minimal_v5(expiry_height);
        let parsed_height =
            parse_transaction_expiry_height(&raw_tx_bytes).expect("parse minimal v5 transaction");
        assert_eq!(parsed_height, BlockHeight::from(expiry_height));
    }

    #[test]
    fn parses_v6_expiry_height() {
        let expiry_height = 4_321_098;
        let raw_tx_bytes = build_minimal_v6(expiry_height);
        let parsed_height =
            parse_transaction_expiry_height(&raw_tx_bytes).expect("parse minimal v6 transaction");
        assert_eq!(parsed_height, BlockHeight::from(expiry_height));
    }

    #[test]
    fn rejects_malformed_transaction_bytes() {
        let error = parse_transaction_expiry_height(&[0_u8; 4])
            .expect_err("malformed transaction bytes must fail");
        let TransactionParseError::Read { reason } = error;
        assert!(!reason.is_empty());
    }

    #[test]
    fn malformed_transaction_is_not_retryable() {
        let error = parse_transaction_expiry_height(&[0_u8; 4])
            .expect_err("malformed transaction bytes must fail");
        assert_eq!(error.posture(), zally_core::FailurePosture::NotRetryable);
    }
}
