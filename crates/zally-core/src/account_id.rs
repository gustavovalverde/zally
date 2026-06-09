//! Opaque identifier for an account within a wallet.

use uuid::Uuid;

/// Opaque identifier for an account within a wallet.
///
/// Account identity is key identity: the default sqlite storage backend computes a
/// UUID v5 of the account's UFVK (encoded for the wallet's network) under a fixed
/// namespace, so
/// the same key material yields the same identifier across database rebuilds, machines,
/// and resets.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AccountId(Uuid);

impl AccountId {
    /// Wraps a raw [`Uuid`] as an `AccountId`.
    ///
    /// Callers outside Zally should obtain an `AccountId` from wallet operations rather than
    /// constructing it from a raw UUID; this constructor exists for storage-impl translation
    /// and for test fixtures.
    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Exposes the underlying [`Uuid`].
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_round_trips_uuid() {
        let uuid = Uuid::new_v4();
        let account_id = AccountId::from_uuid(uuid);
        assert_eq!(account_id.as_uuid(), uuid);
    }
}
