//! Shared error type for canonical hash hex parsing ([`crate::TxId`], [`crate::BlockHash`]).
//!
//! The Zcash protocol specification defines two byte orders for 32-byte hashes
//! (txid, block hash, etc.):
//!
//! - **Internal byte order**: the raw SHA-256d output bytes used in consensus
//!   serialization. Reference: Zcash protocol spec, protocol.tex:13560-13564.
//! - **RPC byte order**: the byte-reversed form every `zcash-cli` reply, every
//!   wallet UI, every block explorer URL, and the specification itself print.
//!   Reference: Zcash protocol spec, term `\rpcByteOrder`
//!   (protocol.tex:1127, defining sentence at :4036).
//!
//! Zally's hash newtypes ([`crate::TxId`], [`crate::BlockHash`]) accept and emit
//! RPC byte order at every text boundary (`Display`, `FromStr`, `to_rpc_hex`,
//! `from_rpc_hex`); their `from_bytes` / `as_bytes` accessors carry internal
//! byte order for storage and consensus-serialization paths.

/// Length in bytes of every Zally hash newtype that uses RPC byte order hex.
pub(crate) const HASH_BYTE_COUNT: usize = 32;

/// Length in lowercase ASCII hex characters of a Zally hash text form.
pub(crate) const HASH_RPC_HEX_LEN: usize = HASH_BYTE_COUNT * 2;

/// Error returned when parsing a Zally hash newtype from RPC byte order hex fails.
///
/// Returned by [`crate::TxId::from_rpc_hex`], [`crate::BlockHash::from_rpc_hex`],
/// and the corresponding [`std::str::FromStr`] implementations.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FromRpcHexError {
    /// Input length did not match the expected number of hex characters.
    ///
    /// `not_retryable`: the caller must supply input of the expected length.
    #[error("expected {expected} hex characters, received {actual}")]
    InvalidLength {
        /// Number of hex characters the target type requires (always 64 today).
        expected: usize,
        /// Number of hex characters the caller supplied.
        actual: usize,
    },

    /// Input contained a non-hex byte.
    ///
    /// `not_retryable`: the caller must replace the offending input.
    #[error("invalid hex input: {source}")]
    InvalidHex {
        /// Underlying hex decode failure from the `hex` crate.
        #[from]
        source: hex::FromHexError,
    },
}

impl FromRpcHexError {
    /// Whether the same input may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::InvalidLength { .. } | Self::InvalidHex { .. } => false,
        }
    }
}

/// Decode an RPC byte order hex string into internal byte order bytes.
///
/// Shared by [`crate::TxId::from_rpc_hex`] and [`crate::BlockHash::from_rpc_hex`].
/// Accepts canonical 64-character hex (lowercase or uppercase), hex-decodes it,
/// then reverses the resulting bytes so the storage form matches the consensus
/// (internal byte order) representation.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
///
/// # Errors
///
/// Returns [`FromRpcHexError::InvalidLength`] when `input.len() != 64`, and
/// [`FromRpcHexError::InvalidHex`] when the bytes are not valid hex.
pub(crate) fn decode_rpc_hex(input: &str) -> Result<[u8; HASH_BYTE_COUNT], FromRpcHexError> {
    if input.len() != HASH_RPC_HEX_LEN {
        return Err(FromRpcHexError::InvalidLength {
            expected: HASH_RPC_HEX_LEN,
            actual: input.len(),
        });
    }
    let mut buffer = [0_u8; HASH_BYTE_COUNT];
    hex::decode_to_slice(input, &mut buffer)?;
    buffer.reverse();
    Ok(buffer)
}

/// Encode internal byte order bytes as RPC byte order hex (lowercase).
///
/// Shared by [`crate::TxId::to_rpc_hex`], [`crate::BlockHash::to_rpc_hex`] and
/// their [`std::fmt::Display`] implementations. Reverses the input then
/// hex-encodes so the leftmost hex character corresponds to the hash's high
/// byte in the form readers recognize.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
#[must_use]
pub(crate) fn encode_rpc_hex(bytes: &[u8; HASH_BYTE_COUNT]) -> String {
    let mut reversed = *bytes;
    reversed.reverse();
    hex::encode(reversed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rpc_hex_round_trip() -> Result<(), FromRpcHexError> {
        let original = [0xAB_u8; HASH_BYTE_COUNT];
        let hex_form = encode_rpc_hex(&original);
        let decoded = decode_rpc_hex(&hex_form)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn decode_rpc_hex_rejects_wrong_length() {
        let outcome = decode_rpc_hex("ab");
        assert!(matches!(
            outcome,
            Err(FromRpcHexError::InvalidLength {
                expected: 64,
                actual: 2,
            })
        ));
        if let Err(err) = outcome {
            assert!(!err.is_retryable());
        }
    }

    #[test]
    fn decode_rpc_hex_rejects_non_hex() {
        let invalid = "z".repeat(HASH_RPC_HEX_LEN);
        assert!(matches!(
            decode_rpc_hex(&invalid),
            Err(FromRpcHexError::InvalidHex { .. })
        ));
    }
}
