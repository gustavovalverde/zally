//! Canonical Zcash block hash newtype.

use core::fmt;
use core::str::FromStr;

use crate::hash_hex::{FromRpcHexError, HASH_BYTE_COUNT, decode_rpc_hex, encode_rpc_hex};

/// Canonical Zcash block hash.
///
/// The byte representation a `BlockHash` holds is **internal byte order**
/// (the consensus serialization form, reference: Zcash protocol spec,
/// protocol.tex:13560-13564). Every text boundary on this type
/// ([`fmt::Display`], [`fmt::Debug`], [`FromStr`], [`Self::to_rpc_hex`],
/// [`Self::from_rpc_hex`]) carries the byte-reversed **RPC byte order** form:
/// the 64-character lowercase hex string that `zcash-cli`, every wallet UI, and
/// every block explorer renders.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder`
/// (protocol.tex:1127, defining sentence at :4036).
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockHash([u8; HASH_BYTE_COUNT]);

impl BlockHash {
    /// Constructs a `BlockHash` from its 32-byte digest in internal byte order.
    ///
    /// Use this on the storage and consensus-serialization seam, where bytes
    /// are already in internal byte order. Text input (RPC byte order hex)
    /// goes through [`Self::from_rpc_hex`] or [`FromStr`].
    #[must_use]
    pub const fn from_bytes(bytes: [u8; HASH_BYTE_COUNT]) -> Self {
        Self(bytes)
    }

    /// Returns the underlying 32-byte digest in internal byte order.
    ///
    /// Use this on the storage and consensus-serialization seam. For text
    /// rendering use [`Self::to_rpc_hex`] (or [`fmt::Display`]).
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; HASH_BYTE_COUNT] {
        &self.0
    }

    /// Returns the block hash as a 64-character lowercase RPC byte order hex string.
    ///
    /// Equivalent to `format!("{self}")` and to the [`FromStr`] inverse
    /// [`Self::from_rpc_hex`]. Produces the canonical form every Zcash
    /// JSON-RPC reply (`getbestblockhash`, `getblock`), wallet UI, and block
    /// explorer renders.
    ///
    /// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
    #[must_use]
    pub fn to_rpc_hex(&self) -> String {
        encode_rpc_hex(&self.0)
    }

    /// Parses a `BlockHash` from a 64-character RPC byte order hex string.
    ///
    /// Inverse of [`Self::to_rpc_hex`]. Accepts canonical 64-character hex
    /// (lowercase or uppercase, matching the `hex` crate's decoder); shorter,
    /// longer, or non-hex input is rejected.
    ///
    /// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
    ///
    /// # Errors
    ///
    /// Returns [`FromRpcHexError::InvalidLength`] when the input is not 64
    /// characters, and [`FromRpcHexError::InvalidHex`] when the characters are
    /// not valid hex.
    pub fn from_rpc_hex(input: &str) -> Result<Self, FromRpcHexError> {
        Ok(Self(decode_rpc_hex(input)?))
    }
}

impl From<[u8; HASH_BYTE_COUNT]> for BlockHash {
    fn from(bytes: [u8; HASH_BYTE_COUNT]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl fmt::Display for BlockHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut reversed = self.0;
        reversed.reverse();
        for byte in reversed {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for BlockHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "BlockHash(\"{self}\")")
    }
}

impl FromStr for BlockHash {
    type Err = FromRpcHexError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::from_rpc_hex(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Internal byte order of a real testnet block hash for height 4031230.
    /// Paired with [`TESTNET_BLOCK_HASH_RPC_HEX`] below; the two are byte-reversed.
    const TESTNET_BLOCK_HASH_INTERNAL_BYTES: [u8; HASH_BYTE_COUNT] = [
        0xee, 0xce, 0xfc, 0x22, 0xf4, 0xa0, 0x9f, 0xe4, 0x30, 0x6f, 0x40, 0xaf, 0xa3, 0xa6, 0xf3,
        0xdb, 0x17, 0x3f, 0x1a, 0x5e, 0x3a, 0x0c, 0xcc, 0x3d, 0x8f, 0xeb, 0x22, 0xc6, 0xba, 0xf1,
        0x33, 0x00,
    ];

    /// RPC byte order hex of the testnet block hash above. Matches the value
    /// `zcash-cli getblockhash 4031230` returns on testnet.
    const TESTNET_BLOCK_HASH_RPC_HEX: &str =
        "0033f1bac622eb8f3dcc0c3a5e1a3f17dbf3a6a3af406f30e49fa0f422fcceee";

    #[test]
    fn block_hash_round_trip() {
        let bytes = [0xCD_u8; HASH_BYTE_COUNT];
        let block_hash = BlockHash::from_bytes(bytes);
        assert_eq!(*block_hash.as_bytes(), bytes);
    }

    #[test]
    fn block_hash_to_rpc_hex_matches_zcash_cli_for_testnet_fixture() {
        let block_hash = BlockHash::from_bytes(TESTNET_BLOCK_HASH_INTERNAL_BYTES);
        assert_eq!(block_hash.to_rpc_hex(), TESTNET_BLOCK_HASH_RPC_HEX);
    }

    #[test]
    fn block_hash_from_rpc_hex_round_trips_against_internal_bytes() -> Result<(), FromRpcHexError> {
        let block_hash = BlockHash::from_rpc_hex(TESTNET_BLOCK_HASH_RPC_HEX)?;
        assert_eq!(block_hash.as_bytes(), &TESTNET_BLOCK_HASH_INTERNAL_BYTES);
        Ok(())
    }

    #[test]
    fn block_hash_text_round_trip_through_rpc_hex() -> Result<(), FromRpcHexError> {
        let original = BlockHash::from_bytes(TESTNET_BLOCK_HASH_INTERNAL_BYTES);
        let rendered = original.to_rpc_hex();
        let decoded = BlockHash::from_rpc_hex(&rendered)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn block_hash_from_rpc_hex_rejects_short_input() {
        let outcome = BlockHash::from_rpc_hex("");
        assert!(matches!(
            outcome,
            Err(FromRpcHexError::InvalidLength {
                expected: 64,
                actual: 0,
            })
        ));
    }

    #[test]
    fn block_hash_from_rpc_hex_rejects_63_characters() {
        let outcome = BlockHash::from_rpc_hex(&"a".repeat(63));
        assert!(matches!(
            outcome,
            Err(FromRpcHexError::InvalidLength {
                expected: 64,
                actual: 63,
            })
        ));
    }

    #[test]
    fn block_hash_from_rpc_hex_rejects_65_characters() {
        let outcome = BlockHash::from_rpc_hex(&"a".repeat(65));
        assert!(matches!(
            outcome,
            Err(FromRpcHexError::InvalidLength {
                expected: 64,
                actual: 65,
            })
        ));
    }

    #[test]
    fn block_hash_from_rpc_hex_rejects_non_hex_characters() {
        let outcome = BlockHash::from_rpc_hex(&"z".repeat(64));
        assert!(matches!(outcome, Err(FromRpcHexError::InvalidHex { .. })));
    }

    #[test]
    fn block_hash_from_rpc_hex_accepts_uppercase_per_hex_crate() -> Result<(), FromRpcHexError> {
        let lower = BlockHash::from_rpc_hex(TESTNET_BLOCK_HASH_RPC_HEX)?;
        let upper = BlockHash::from_rpc_hex(&TESTNET_BLOCK_HASH_RPC_HEX.to_uppercase())?;
        assert_eq!(lower, upper);
        Ok(())
    }

    #[test]
    fn block_hash_display_equals_to_rpc_hex() {
        let block_hash = BlockHash::from_bytes(TESTNET_BLOCK_HASH_INTERNAL_BYTES);
        assert_eq!(format!("{block_hash}"), block_hash.to_rpc_hex());
    }

    #[test]
    fn block_hash_from_str_equals_from_rpc_hex() -> Result<(), FromRpcHexError> {
        let parsed: BlockHash = TESTNET_BLOCK_HASH_RPC_HEX.parse()?;
        let expected = BlockHash::from_rpc_hex(TESTNET_BLOCK_HASH_RPC_HEX)?;
        assert_eq!(parsed, expected);
        Ok(())
    }

    #[test]
    fn block_hash_debug_renders_rpc_hex() {
        let block_hash = BlockHash::from_bytes(TESTNET_BLOCK_HASH_INTERNAL_BYTES);
        let debug = format!("{block_hash:?}");
        assert_eq!(
            debug,
            format!("BlockHash(\"{TESTNET_BLOCK_HASH_RPC_HEX}\")")
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn block_hash_serde_round_trips_through_json_bytes() -> Result<(), serde_json::Error> {
        let original = BlockHash::from_bytes(TESTNET_BLOCK_HASH_INTERNAL_BYTES);
        let json = serde_json::to_string(&original)?;
        let decoded: BlockHash = serde_json::from_str(&json)?;
        assert_eq!(decoded, original);
        assert_eq!(decoded.to_rpc_hex(), TESTNET_BLOCK_HASH_RPC_HEX);
        Ok(())
    }
}
