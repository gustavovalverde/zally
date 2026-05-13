//! Non-malleable transaction identifier per ZIP-244.

/// Non-malleable transaction identifier per ZIP-244.
///
/// Always the `txid_digest` field, never the legacy SHA-256d txid.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TxId([u8; 32]);

impl TxId {
    /// Constructs a `TxId` from its 32-byte digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the underlying 32-byte digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for TxId {
    fn from(bytes: [u8; 32]) -> Self {
        Self::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txid_round_trip() {
        let bytes = [0xAB_u8; 32];
        let txid = TxId::from_bytes(bytes);
        assert_eq!(*txid.as_bytes(), bytes);
    }
}
