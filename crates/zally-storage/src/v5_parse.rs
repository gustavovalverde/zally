//! Read-side counterpart to the v5 transaction serialization performed by
//! [`crate::sqlite::Sqlite::extract_and_store_pczt`].
//!
//! Both sides of the v5 wire format live in this crate so downstream consumers
//! (zpay `/settle`, broadcast workers, indexers) do not pin a separate
//! `zcash_primitives` version. A version skew across crate boundaries makes
//! valid signed bytes look malformed at parse time; owning both sides here is
//! the single root that the rest of the workspace anchors to.
//!
//! The public surface deliberately wraps `zcash_primitives::transaction::Transaction`
//! so the ABI stays stable when upstream bumps.
//!
//! # Symmetry with the write path
//!
//! The companion site is `zally_storage::sqlite::extract_and_store_pczt`, which
//! calls `stored.write(&mut raw_bytes)` and then records
//! `stored.expiry_height()`. This module reverses that pair: read the bytes,
//! recover the expiry height.
//!
//! ```text
//! WRITE: Transaction          -> Vec<u8>     (sqlite::extract_and_store_pczt)
//! READ:  &[u8]                -> u32         (v5_parse::parse_v5_expiry_height)
//! ```

use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BranchId;

/// Error returned by [`parse_v5_expiry_height`].
///
/// The variant set is intentionally narrow: any failure to decode the v5 wire
/// format collapses to [`V5ParseError::Read`] with a stringified reason. This
/// keeps the public ABI stable when upstream `zcash_primitives` renames or
/// restructures its internal IO errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum V5ParseError {
    /// The supplied bytes were not a parseable v5 Zcash transaction.
    ///
    /// Wraps the underlying IO or decode failure. The string is operator-facing
    /// and intentionally not machine-introspected.
    #[error("v5 transaction decode failed: {reason}")]
    Read {
        /// Underlying error description from `zcash_primitives::transaction::Transaction::read`.
        reason: String,
    },
}

/// Parses a serialized v5 Zcash transaction and returns its `expiry_height`.
///
/// Symmetric with the `Transaction::write` call in
/// `zally_storage::sqlite::extract_and_store_pczt`. This crate owns both sides
/// of the v5 wire format so downstream consumers (zpay `/settle`, etc.) do not
/// pin a separate `zcash_primitives`.
///
/// The branch id passed to `Transaction::read` is [`BranchId::Nu5`]. For v5
/// transactions the value is effectively ignored: the v5 reader consumes the
/// consensus branch id from the wire header. The parameter is only consulted
/// for v3/v4 transactions, which this function is not designed to parse.
///
/// # Errors
///
/// Returns [`V5ParseError::Read`] when the input is not a parseable Zcash
/// transaction (truncated bytes, unknown version, malformed bundle, etc.).
pub fn parse_v5_expiry_height(raw_bytes: &[u8]) -> Result<u32, V5ParseError> {
    let tx = Transaction::read(raw_bytes, BranchId::Nu5).map_err(|err| V5ParseError::Read {
        reason: err.to_string(),
    })?;
    Ok(u32::from(tx.expiry_height()))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "test helpers panic on impossible failures (minimal v5 with empty bundles always freezes and writes)"
)]
mod tests {
    use super::*;
    use zcash_primitives::transaction::{TransactionData, TxVersion};
    use zcash_protocol::consensus::BlockHeight;

    /// Builds a minimal but well-formed v5 transaction with the given expiry.
    ///
    /// Empty bundles mirror what an unfunded coinbase-shape v5 looks like on
    /// the wire and exercise the same `read_v5` codepath as a real signed PCZT
    /// extraction.
    fn build_minimal_v5(expiry: u32) -> Vec<u8> {
        let data = TransactionData::from_parts(
            TxVersion::V5,
            BranchId::Nu5,
            /* lock_time */ 0,
            BlockHeight::from(expiry),
            /* transparent_bundle */ None,
            /* sprout_bundle */ None,
            /* sapling_bundle */ None,
            /* orchard_bundle */ None,
        );
        let tx = data.freeze().expect("freeze minimal v5");
        let mut bytes = Vec::new();
        tx.write(&mut bytes).expect("write minimal v5");
        bytes
    }

    #[test]
    fn round_trip_recovers_expiry_height() {
        let expected_expiry: u32 = 4_321_098;
        let bytes = build_minimal_v5(expected_expiry);
        let parsed = parse_v5_expiry_height(&bytes).expect("parse round-tripped v5");
        assert_eq!(parsed, expected_expiry);
    }

    #[test]
    fn round_trip_handles_zero_expiry() {
        let bytes = build_minimal_v5(0);
        let parsed = parse_v5_expiry_height(&bytes).expect("parse v5 with zero expiry");
        assert_eq!(parsed, 0);
    }

    #[test]
    fn malformed_input_returns_read_error() {
        let err = parse_v5_expiry_height(&[0u8; 4]).expect_err("expected decode failure");
        let V5ParseError::Read { reason } = err;
        assert!(
            !reason.is_empty(),
            "V5ParseError::Read should carry a non-empty reason"
        );
    }

    #[test]
    fn empty_input_returns_read_error() {
        let err = parse_v5_expiry_height(&[]).expect_err("expected decode failure on empty input");
        assert!(matches!(err, V5ParseError::Read { .. }));
    }
}
