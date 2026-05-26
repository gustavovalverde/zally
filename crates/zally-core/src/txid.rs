//! Non-malleable transaction identifier per ZIP-244.

use core::fmt;
use core::str::FromStr;

use crate::hash_hex::{FromRpcHexError, HASH_BYTE_COUNT, decode_rpc_hex, encode_rpc_hex};

/// Non-malleable transaction identifier per ZIP-244.
///
/// Always the `txid_digest` field, never the legacy SHA-256d txid. The byte
/// representation a `TxId` holds is **internal byte order** (the consensus
/// serialization form, reference: Zcash protocol spec, protocol.tex:13560-13564).
/// Every text boundary on this type ([`fmt::Display`], [`fmt::Debug`],
/// [`FromStr`], [`Self::to_rpc_hex`], [`Self::from_rpc_hex`]) carries the
/// byte-reversed **RPC byte order** form: the 64-character lowercase hex string
/// that `zcash-cli`, every wallet UI, and every block explorer renders.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder`
/// (protocol.tex:1127, defining sentence at :4036).
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TxId([u8; HASH_BYTE_COUNT]);

impl TxId {
    /// Constructs a `TxId` from its 32-byte digest in internal byte order.
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

    /// Returns the txid as a 64-character lowercase RPC byte order hex string.
    ///
    /// Equivalent to `format!("{self}")` and to the [`FromStr`] inverse
    /// [`Self::from_rpc_hex`]. Produces the canonical form every Zcash
    /// JSON-RPC reply, wallet UI, and block explorer renders.
    ///
    /// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
    #[must_use]
    pub fn to_rpc_hex(&self) -> String {
        encode_rpc_hex(&self.0)
    }

    /// Parses a `TxId` from a 64-character RPC byte order hex string.
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

impl From<[u8; HASH_BYTE_COUNT]> for TxId {
    fn from(bytes: [u8; HASH_BYTE_COUNT]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl fmt::Display for TxId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut reversed = self.0;
        reversed.reverse();
        for byte in reversed {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for TxId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "TxId(\"{self}\")")
    }
}

impl FromStr for TxId {
    type Err = FromRpcHexError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::from_rpc_hex(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Internal byte order of a real testnet txid mined in block 4031230.
    /// Paired with [`TESTNET_TXID_RPC_HEX`] below; the two are byte-reversed.
    const TESTNET_TXID_INTERNAL_BYTES: [u8; HASH_BYTE_COUNT] = [
        0x36, 0x94, 0x55, 0xb7, 0x8a, 0xfc, 0xa3, 0xdc, 0xb5, 0x2b, 0xec, 0xfd, 0x38, 0x72, 0xba,
        0xf5, 0xd0, 0x51, 0xb3, 0x2e, 0x81, 0x65, 0xbc, 0x2c, 0x79, 0x61, 0x06, 0x9e, 0xe6, 0x0c,
        0xca, 0xc3,
    ];

    /// RPC byte order hex of the testnet txid above. Matches the value
    /// `zcash-cli getblock` returns for the txid at testnet height 4031230.
    const TESTNET_TXID_RPC_HEX: &str =
        "c3ca0ce69e0661792cbc65812eb351d0f5ba7238fdec2bb5dca3fc8ab7559436";

    #[test]
    fn txid_round_trip() {
        let bytes = [0xAB_u8; HASH_BYTE_COUNT];
        let txid = TxId::from_bytes(bytes);
        assert_eq!(*txid.as_bytes(), bytes);
    }

    #[test]
    fn txid_to_rpc_hex_matches_zcash_cli_for_testnet_fixture() {
        let txid = TxId::from_bytes(TESTNET_TXID_INTERNAL_BYTES);
        assert_eq!(txid.to_rpc_hex(), TESTNET_TXID_RPC_HEX);
    }

    #[test]
    fn txid_from_rpc_hex_round_trips_against_internal_bytes() -> Result<(), FromRpcHexError> {
        let txid = TxId::from_rpc_hex(TESTNET_TXID_RPC_HEX)?;
        assert_eq!(txid.as_bytes(), &TESTNET_TXID_INTERNAL_BYTES);
        Ok(())
    }

    #[test]
    fn txid_text_round_trip_through_rpc_hex() -> Result<(), FromRpcHexError> {
        let original = TxId::from_bytes(TESTNET_TXID_INTERNAL_BYTES);
        let rendered = original.to_rpc_hex();
        let decoded = TxId::from_rpc_hex(&rendered)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn txid_from_rpc_hex_rejects_short_input() {
        let outcome = TxId::from_rpc_hex("");
        assert!(matches!(
            outcome,
            Err(FromRpcHexError::InvalidLength {
                expected: 64,
                actual: 0,
            })
        ));
    }

    #[test]
    fn txid_from_rpc_hex_rejects_63_characters() {
        let outcome = TxId::from_rpc_hex(&"a".repeat(63));
        assert!(matches!(
            outcome,
            Err(FromRpcHexError::InvalidLength {
                expected: 64,
                actual: 63,
            })
        ));
    }

    #[test]
    fn txid_from_rpc_hex_rejects_65_characters() {
        let outcome = TxId::from_rpc_hex(&"a".repeat(65));
        assert!(matches!(
            outcome,
            Err(FromRpcHexError::InvalidLength {
                expected: 64,
                actual: 65,
            })
        ));
    }

    #[test]
    fn txid_from_rpc_hex_rejects_non_hex_characters() {
        let outcome = TxId::from_rpc_hex(&"z".repeat(64));
        assert!(matches!(outcome, Err(FromRpcHexError::InvalidHex { .. })));
    }

    #[test]
    fn txid_from_rpc_hex_accepts_uppercase_per_hex_crate() -> Result<(), FromRpcHexError> {
        let lower = TxId::from_rpc_hex(TESTNET_TXID_RPC_HEX)?;
        let upper = TxId::from_rpc_hex(&TESTNET_TXID_RPC_HEX.to_uppercase())?;
        assert_eq!(lower, upper);
        Ok(())
    }

    #[test]
    fn txid_display_equals_to_rpc_hex() {
        let txid = TxId::from_bytes(TESTNET_TXID_INTERNAL_BYTES);
        assert_eq!(format!("{txid}"), txid.to_rpc_hex());
    }

    #[test]
    fn txid_from_str_equals_from_rpc_hex() -> Result<(), FromRpcHexError> {
        let parsed: TxId = TESTNET_TXID_RPC_HEX.parse()?;
        let expected = TxId::from_rpc_hex(TESTNET_TXID_RPC_HEX)?;
        assert_eq!(parsed, expected);
        Ok(())
    }

    #[test]
    fn txid_debug_renders_rpc_hex() {
        let txid = TxId::from_bytes(TESTNET_TXID_INTERNAL_BYTES);
        let debug = format!("{txid:?}");
        assert_eq!(debug, format!("TxId(\"{TESTNET_TXID_RPC_HEX}\")"));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn txid_serde_round_trips_through_json_bytes() -> Result<(), serde_json::Error> {
        let original = TxId::from_bytes(TESTNET_TXID_INTERNAL_BYTES);
        let json = serde_json::to_string(&original)?;
        let decoded: TxId = serde_json::from_str(&json)?;
        assert_eq!(decoded, original);
        assert_eq!(decoded.to_rpc_hex(), TESTNET_TXID_RPC_HEX);
        Ok(())
    }
}
