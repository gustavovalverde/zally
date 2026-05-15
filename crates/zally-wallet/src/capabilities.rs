//! Wallet capability descriptor.

use std::collections::BTreeSet;

use zally_core::Network;

/// Runtime descriptor of supported wallet features.
///
/// Integrations read this at runtime to feature-detect supported sealing implementations, storage
/// backends, and protocol coverage without pinning a Zally version. New capabilities are
/// additive enum variants under `#[non_exhaustive]`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct WalletCapabilities {
    /// Network the wallet is bound to.
    pub network: Network,
    /// Sealing implementation in use.
    pub sealing: SealingCapability,
    /// Storage backend in use.
    pub storage: StorageCapability,
    /// Protocol features advertised by this wallet build.
    pub features: BTreeSet<Capability>,
}

/// The sealing implementation behind a [`crate::Wallet`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SealingCapability {
    /// Age-encrypted file sealing (`zally_keys::AgeFileSealing`).
    AgeFile,
    /// In-memory sealing (`zally_testkit::InMemorySealing`). Tests only.
    InMemory,
    /// Plaintext seed storage. Available only behind the `unsafe_plaintext_seed` feature.
    #[cfg(feature = "unsafe_plaintext_seed")]
    Plaintext,
    /// A custom sealing implementation provided by the operator.
    Custom,
}

/// The storage backend behind a [`crate::Wallet`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum StorageCapability {
    /// `zally_storage::SqliteWalletStorage`.
    Sqlite,
    /// A custom storage implementation provided by the operator.
    Custom,
}

/// A protocol capability advertised by Zally.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Capability {
    /// ZIP-316 Unified Addresses.
    Zip316UnifiedAddresses,
    /// ZIP-302 memo encoding and the memo-on-transparent guard.
    Zip302Memos,
    /// ZIP-320 TEX address recognition at the API boundary.
    Zip320TexAddresses,
    /// ZIP-317 conventional-fee default.
    Zip317ConventionalFee,
    /// `Wallet::sync` is available.
    SyncIncremental,
    /// `SyncDriver` is available for caller-owned continuous sync.
    SyncDriver,
    /// `Wallet::observe` is available.
    EventStream,
    /// `Wallet::send_payment` honours the caller-supplied `zally_core::IdempotencyKey`.
    IdempotentSend,
    /// PCZT v0.6 export and import via `zally-pczt`.
    PcztV06,
    /// `Wallet::metrics_snapshot` returns a typed [`crate::WalletMetrics`].
    MetricsSnapshot,
    /// `Wallet::status_snapshot` returns a typed [`crate::WalletStatus`].
    StatusSnapshot,
    /// The wallet's [`crate::CircuitBreaker`] has tripped open. Subsequent outbound IO
    /// short-circuits with [`crate::WalletError::CircuitBroken`] until the breaker cools
    /// down. Cleared automatically when the breaker re-closes.
    CircuitBroken,
}
