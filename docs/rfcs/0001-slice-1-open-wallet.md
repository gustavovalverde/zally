# RFC 0001: Slice 1 — Open a Wallet

| Field | Value |
|---|---|
| Status | Accepted |
| Product | Zally |
| Slice | 1 |
| Related | [PRD-0001](../prd-0001-zally-headless-wallet-library.md), [ADR-0001](../adrs/0001-workspace-crate-boundaries.md), [ADR-0002](../adrs/0002-founding-implementation-patterns.md) |
| Created | 2026-05-12 |
| Revised | 2026-05-12 (architectural refinement; see §10) |

## Summary

Slice 1 lands the minimal vertical stack required to open a wallet: seal a seed at rest using age encryption, open or create a SQLite-backed wallet database, run schema migrations, derive the first Unified Address for the first account, and prove persistence by re-opening the same sealed seed and asserting the derived address is identical. Five crates ship together: `zally-core`, `zally-keys`, `zally-storage`, `zally-wallet`, and `zally-testkit`. Slice 1 deliberately defers all chain interaction (`zally-chain`, `zally-pczt`), the scan loop (`zally-wallet::sync`), sending (`zally-wallet::send`), and every REQ-CHAIN, REQ-SYNC, REQ-SPEND, and REQ-PCZT requirement to later slices. The `WalletCapabilities` and `WalletStorage` trait surfaces defined here are designed for additive extension; no breaking change is expected.

The §8 open questions are resolved. §10 records the architectural refinements that distinguish this revision from the initial draft.

---

## 1. Crate Layout

### 1.1 `zally-core`

**Role**: Domain types. Zero domain-foreign dependencies (the only librustzcash deps are `zcash_protocol` for `Parameters`, `BranchId`, `LocalNetwork`, and `Memo`). Every other crate depends on it.

**File list**:

| File | Primary export |
|---|---|
| `src/lib.rs` | crate root; re-exports all public items |
| `src/network.rs` | `Network`, `NetworkParameters` |
| `src/zatoshis.rs` | `Zatoshis`, `ZatoshisError` |
| `src/block_height.rs` | `BlockHeight` |
| `src/branch_id.rs` | `BranchId` re-export from `zcash_protocol` |
| `src/txid.rs` | `TxId` |
| `src/account_id.rs` | `AccountId` |
| `src/idempotency_key.rs` | `IdempotencyKey`, `IdempotencyKeyError` |
| `src/memo.rs` | `Memo`, `MemoBytes`, `MemoError` (all re-exported from `zcash_protocol::memo`) |

### 1.2 `zally-keys`

**Role**: Seed lifecycle, mnemonic generation, `SeedSealing` trait, UFVK derivation.

| File | Primary export |
|---|---|
| `src/lib.rs` | crate root |
| `src/seed_material.rs` | `SeedMaterial`, `SeedMaterialError` |
| `src/mnemonic.rs` | `Mnemonic`, `MnemonicError` |
| `src/sealing.rs` | `SeedSealing` trait, `SealingError` |
| `src/age_file_sealing.rs` | `AgeFileSealing`, `AgeFileSealingOptions` |
| `src/plaintext_sealing.rs` | `PlaintextSealing` (feature `unsafe_plaintext_seed`) |
| `src/ufvk.rs` | `derive_ufvk` |

Cargo features:

- `unsafe_plaintext_seed` — exposes `PlaintextSealing`. Never enable in production binaries.
- `serde` — derives `Serialize`/`Deserialize` on public types where applicable (delegated through `zally-core`).

### 1.3 `zally-storage`

**Role**: `WalletStorage` trait wrapping librustzcash's `WalletRead`/`WalletWrite`; `SqliteWalletStorage` implementation.

| File | Primary export |
|---|---|
| `src/lib.rs` | crate root |
| `src/wallet_storage.rs` | `WalletStorage` trait |
| `src/sqlite.rs` | `SqliteWalletStorage`, `SqliteWalletStorageOptions` |
| `src/storage_error.rs` | `StorageError` |

No `AccountMap`. `zally_core::AccountId(Uuid)` translates to `zcash_client_sqlite::AccountUuid` by the identity function (both wrap `uuid::Uuid`). Future backends with non-UUID native ids own their own translation; the sqlite backend does not.

### 1.4 `zally-wallet`

**Role**: Operator-facing API. `Wallet`, `WalletCapabilities`.

| File | Primary export |
|---|---|
| `src/lib.rs` | crate root |
| `src/wallet.rs` | `Wallet`, `Wallet::create`, `Wallet::open` |
| `src/capabilities.rs` | `WalletCapabilities`, `SealingCapability`, `StorageCapability` |
| `src/wallet_error.rs` | `WalletError` |

Cargo features:

- `unsafe_plaintext_seed` — forwards to `zally-keys/unsafe_plaintext_seed`.
- `serde` — gates `Serialize`/`Deserialize` on `WalletCapabilities`, `SealingCapability`, `StorageCapability` and forwards to `zally-core/serde` and `zally-keys/serde`.

### 1.5 `zally-testkit`

**Role**: Fixtures for tests in other crates. `publish = false`.

| File | Primary export |
|---|---|
| `src/lib.rs` | crate root |
| `src/in_memory_sealing.rs` | `InMemorySealing` (shared-state semantics for round-trip tests) |
| `src/temp_wallet_path.rs` | `TempWalletPath` |
| `src/live.rs` | `init()`, `require_live()`, `LIVE_TEST_IGNORE_REASON`, `LiveTestError` |

---

## 2. Public Surface

### 2.1 `zally-core`

#### `Network`

```rust
/// Zcash network variant.
///
/// `Regtest` carries `zcash_protocol::local_consensus::LocalNetwork` directly; there is no
/// Zally-side duplicate of the upstream regtest-parameters type. Convenience constructors
/// build the common shapes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Network {
    Mainnet,
    Testnet,
    Regtest(zcash_protocol::local_consensus::LocalNetwork),
}

impl Network {
    /// Returns a regtest network with every upgrade activated at height 1.
    ///
    /// Matches the common `nuparams=...:1` configuration used by Zcash regtest nodes.
    #[must_use]
    pub const fn regtest_all_at_genesis() -> Self;

    /// Returns `NetworkParameters`, the opaque `Parameters` impl Zally hands to librustzcash.
    ///
    /// Callers should treat the return value as opaque: pass it where a `Parameters` bound is
    /// required, do not match on it.
    #[must_use]
    pub fn to_parameters(self) -> NetworkParameters;

    /// SLIP-44 coin type. `133` for mainnet, `1` for testnet and regtest.
    #[must_use]
    pub const fn coin_type(self) -> u32;
}

/// Opaque Zcash consensus parameters.
///
/// Implements `zcash_protocol::consensus::Parameters`. Constructed via `Network::to_parameters`.
#[derive(Clone, Copy, Debug)]
pub struct NetworkParameters {
    /* private */
}
```

Rationale: `LocalNetwork` already carries the activation heights with `Option<BlockHeight>` per upgrade and implements `Parameters`. A Zally newtype with the same fields would duplicate the upstream surface and add a maintenance burden on every future network upgrade (`nu7`, `z_future`). Wrapping `LocalNetwork` inside `Network::Regtest` keeps the spine ("network-tagged everything") and avoids the duplicate.

#### `Zatoshis`

```rust
/// Non-negative integer zatoshi amount.
///
/// Refuses construction above `MAX_MONEY` (2_100_000_000_000_000 zatoshis).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Zatoshis(u64);

impl Zatoshis {
    pub const MAX: Self = Zatoshis(2_100_000_000_000_000);

    #[must_use]
    pub const fn as_u64(self) -> u64;

    pub const fn zero() -> Self;
}

impl TryFrom<u64> for Zatoshis {
    type Error = ZatoshisError;
    fn try_from(zat: u64) -> Result<Self, Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ZatoshisError {
    /// not_retryable.
    #[error("zatoshi amount {attempted_zat} exceeds MAX_MONEY (2100000000000000 zatoshis)")]
    ExceedsMaxMoney { attempted_zat: u64 },
}

impl ZatoshisError {
    #[must_use]
    pub const fn is_retryable(&self) -> bool { false }
}
```

#### `BlockHeight`

```rust
/// A Zcash block height.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlockHeight(u32);

impl BlockHeight {
    pub const GENESIS: Self = BlockHeight(0);

    #[must_use]
    pub const fn as_u32(self) -> u32;
}

impl From<u32> for BlockHeight { /* ... */ }
impl From<BlockHeight> for u32 { /* ... */ }
```

Conversion to/from `zcash_protocol::consensus::BlockHeight` is `pub(crate)`-only inside the crates that need it.

#### `BranchId`, `TxId`, `AccountId`

```rust
pub use zcash_protocol::consensus::BranchId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TxId([u8; 32]);

impl TxId {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32];

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self;
}

/// Opaque identifier for an account within a wallet.
///
/// Backed by a UUID v4. For the sqlite storage backend, translation to
/// `zcash_client_sqlite::AccountUuid` is the identity function on the inner `Uuid`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AccountId(uuid::Uuid);

impl AccountId {
    #[must_use]
    pub fn from_uuid(uuid: uuid::Uuid) -> Self;

    #[must_use]
    pub fn as_uuid(self) -> uuid::Uuid;
}
```

#### `IdempotencyKey`

```rust
/// Caller-supplied idempotency key for send operations.
///
/// Must be 1-128 ASCII printable characters (0x20-0x7E inclusive).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    #[must_use]
    pub fn as_str(&self) -> &str;
}

impl TryFrom<&str> for IdempotencyKey {
    type Error = IdempotencyKeyError;
    fn try_from(s: &str) -> Result<Self, Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum IdempotencyKeyError {
    /// not_retryable.
    #[error("idempotency key length is {byte_count}; valid range is 1-128 characters")]
    InvalidLength { byte_count: usize },

    /// not_retryable.
    #[error("idempotency key has a non-printable character at byte offset {byte_offset}; valid range is 0x20-0x7E")]
    InvalidCharacter { byte_offset: usize },
}
```

The error messages name the valid input range so operators and agents can fix offending input from the `Display` output alone.

#### `Memo`

```rust
pub use zcash_protocol::memo::{Memo, MemoBytes, MemoError};
```

Slice 1 re-exports the upstream type verbatim. Re-implementing the enum field-by-field would duplicate the upstream surface and the ZIP-302 encoding rules; the upstream type is the canonical Zcash memo.

`MemoError` is `zcash_protocol::memo::Error`, re-exported as `MemoError` for Zally namespace consistency. Its retry posture is documented in `docs/reference/error-vocabulary.md` rather than added to the upstream type.

---

### 2.2 `zally-keys`

#### `SeedMaterial`

```rust
/// Zeroizing wrapper around raw seed bytes (32-252 bytes per ZIP-32).
pub struct SeedMaterial(secrecy::SecretBox<Vec<u8>>);

impl SeedMaterial {
    /// Derives seed bytes from a `Mnemonic` with the given passphrase.
    ///
    /// not_retryable: derivation is deterministic.
    pub fn from_mnemonic(mnemonic: &Mnemonic, passphrase: &str) -> Self;

    /// Returns the raw seed bytes for use with librustzcash derivation calls.
    pub fn expose_secret(&self) -> &[u8];

    /// Length of the seed in bytes.
    #[must_use]
    pub fn byte_count(&self) -> usize;
}
```

#### `Mnemonic`

```rust
/// BIP-39 mnemonic. Zally-owned wrapper over the underlying BIP-39 crate.
///
/// All Slice 1 mnemonics are 24 words (256 bits of entropy).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mnemonic {
    /* private */
}

impl Mnemonic {
    /// Generates a fresh 24-word mnemonic from the OS RNG.
    #[must_use]
    pub fn generate() -> Self;

    /// Reconstructs a `Mnemonic` from its written phrase.
    ///
    /// Validates wordlist membership and BIP-39 checksum.
    /// not_retryable: a phrase that fails today will fail every time.
    pub fn from_phrase(phrase: &str) -> Result<Self, MnemonicError>;

    /// The space-separated mnemonic phrase. Treat this as sensitive.
    #[must_use]
    pub fn as_phrase(&self) -> &str;

    /// Number of words in the phrase.
    #[must_use]
    pub fn word_count(&self) -> usize;
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum MnemonicError {
    /// not_retryable.
    #[error("mnemonic phrase is invalid: {reason}")]
    InvalidPhrase { reason: String },
}
```

Rationale for the wrapper: the underlying BIP-39 crate (`bip0039 = "0.12"`, matching the librustzcash workspace pin) is an implementation detail. Operators see a Zally type they can serialise (with the `serde` feature on `zally-core`), pattern-match, and use as a return value from `Wallet::create`. A future bump to `bip0039 = "0.14"` or a switch to another BIP-39 crate does not change the public surface.

#### `SeedSealing`

```rust
/// Trait for at-rest seed encryption.
///
/// `SeedSealing` is network-agnostic: a sealed seed is just a seed. Network binding lives on
/// the wallet handle and the storage backend.
#[async_trait::async_trait]
pub trait SeedSealing: Send + Sync + 'static {
    /// Encrypts and persists `seed`. Idempotent.
    ///
    /// retryable on transient I/O. requires_operator on key material errors.
    async fn seal_seed(&self, seed: &SeedMaterial) -> Result<(), SealingError>;

    /// Decrypts and returns the sealed seed material.
    ///
    /// retryable on transient I/O. requires_operator on integrity failure.
    /// not_retryable on `NoSealedSeed`.
    async fn unseal_seed(&self) -> Result<SeedMaterial, SealingError>;
}
```

Methods take `&self` per ADR-0002 Decision 1. Implementations own their concurrency strategy.

#### `AgeFileSealing`

```rust
/// Age-encrypted file sealing.
///
/// Encrypts the seed with a freshly-generated age X25519 identity on first `seal_seed`.
/// The identity is stored alongside the encrypted seed in a sidecar file at
/// `<seed_path>.age-identity`. Both files are created atomically via write-then-rename.
pub struct AgeFileSealing { /* ... */ }

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AgeFileSealingOptions {
    pub seed_path: std::path::PathBuf,
}

impl AgeFileSealingOptions {
    #[must_use]
    pub fn at_path(seed_path: std::path::PathBuf) -> Self;
}

impl AgeFileSealing {
    #[must_use]
    pub fn new(options: AgeFileSealingOptions) -> Self;
}

#[async_trait::async_trait]
impl SeedSealing for AgeFileSealing { /* ... */ }
```

No `network` field. The sealed seed is network-agnostic; the wallet handle and storage carry the network.

#### `PlaintextSealing`

```rust
#[cfg(feature = "unsafe_plaintext_seed")]
/// Plaintext seed storage. NEVER USE IN PRODUCTION.
pub struct PlaintextSealing { /* ... */ }

#[cfg(feature = "unsafe_plaintext_seed")]
impl PlaintextSealing {
    #[must_use]
    pub fn new(seed_path: std::path::PathBuf) -> Self;
}

#[cfg(feature = "unsafe_plaintext_seed")]
#[async_trait::async_trait]
impl SeedSealing for PlaintextSealing { /* silent impl */ }
```

The `PlaintextSealing` impl emits no warning itself. The warning is emitted at the wallet layer based on `Wallet::capabilities().sealing == SealingCapability::Plaintext`, fired exactly once per wallet open. See §3.

#### `SealingError`

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SealingError {
    /// retryable: transient I/O.
    #[error("seed file read failed: {reason}")]
    ReadFailed { reason: String },

    /// retryable: transient I/O.
    #[error("seed file write failed: {reason}")]
    WriteFailed { reason: String },

    /// not_retryable: switch to `Wallet::create` for first-time bootstrap.
    #[error("no sealed seed found at the configured path")]
    NoSealedSeed,

    /// requires_operator: corrupt or missing identity file.
    #[error("age identity error: {reason}")]
    AgeIdentityFailed { reason: String },

    /// requires_operator: integrity failure; the sealed file may be corrupt or the
    /// identity may not match the encrypted seed.
    #[error("age decryption failed: {reason}")]
    DecryptionFailed { reason: String },

    /// requires_operator: the sealed file stores invalid key material.
    #[error("unsealed seed length is {byte_count}; ZIP-32 requires 32-252 bytes")]
    InvalidSeedLength { byte_count: usize },
}

impl SealingError {
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::ReadFailed { .. } | Self::WriteFailed { .. } => true,
            Self::NoSealedSeed
            | Self::AgeIdentityFailed { .. }
            | Self::DecryptionFailed { .. }
            | Self::InvalidSeedLength { .. } => false,
        }
    }
}
```

Per the refined ADR-0002 Decision 5, every variant's retry posture is uniform by construction site, so the `is_retryable: bool` field is dropped. The method body encodes the posture; the rustdoc above the variant names it. Adding a variant whose posture genuinely varies (e.g., a future `BackendError { reason, is_retryable: bool }`) is allowed; the field shape is reserved for that case.

#### `derive_ufvk`

```rust
/// Derives a `UnifiedFullViewingKey` for `account_index` on `network`.
///
/// The intermediate `UnifiedSpendingKey` is dropped (and zeroized) before this function returns.
///
/// not_retryable: derivation is deterministic.
pub fn derive_ufvk(
    network: zally_core::Network,
    seed: &SeedMaterial,
    account_index: zip32::AccountId,
) -> Result<zcash_keys::keys::UnifiedFullViewingKey, KeyDerivationError>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KeyDerivationError {
    /// not_retryable.
    #[error("unified key derivation failed: {reason}")]
    DerivationFailed { reason: String },
}

impl KeyDerivationError {
    #[must_use]
    pub const fn is_retryable(&self) -> bool { false }
}
```

---

### 2.3 `zally-storage`

#### `WalletStorage` (Slice 1 subset)

```rust
/// Trait abstracting the wallet database.
///
/// Slice 1 exposes the methods needed to open, create, and derive the first address. Later
/// slices extend with sync, balance, spend, and event APIs.
#[async_trait::async_trait]
pub trait WalletStorage: Send + Sync + 'static {
    /// Opens or creates the wallet database at the location described by the impl's options.
    /// Runs schema migrations before returning. Idempotent.
    ///
    /// retryable on transient I/O. requires_operator on schema migration failure.
    async fn open_or_create(&self) -> Result<(), StorageError>;

    /// Creates the wallet's first account for `seed` at the given `birthday`.
    ///
    /// Slice 1's invariant is one account per wallet: a second call returns
    /// `StorageError::AccountAlreadyExists`.
    ///
    /// not_retryable on existing account. retryable on transient I/O. requires_operator on
    /// migration mismatch.
    async fn create_account_for_seed(
        &self,
        seed: &zally_keys::SeedMaterial,
        birthday: zally_core::BlockHeight,
    ) -> Result<zally_core::AccountId, StorageError>;

    /// Looks up the `AccountId` for the account whose UFVK matches `seed`.
    ///
    /// Returns `None` if no account matches.
    ///
    /// not_retryable.
    async fn find_account_for_seed(
        &self,
        seed: &zally_keys::SeedMaterial,
    ) -> Result<Option<zally_core::AccountId>, StorageError>;

    /// Generates, persists, and marks as exposed the next-available Unified Address for
    /// `account_id`. Repeated calls walk forward through diversifier indices.
    ///
    /// not_retryable on unknown account. retryable on transient I/O.
    async fn derive_next_address(
        &self,
        account_id: zally_core::AccountId,
    ) -> Result<zcash_keys::address::UnifiedAddress, StorageError>;

    /// Returns the network this storage instance was opened for.
    fn network(&self) -> zally_core::Network;
}
```

No associated `Error` type. The trait canonicalises on `StorageError`. Backends that own a different error type (a hypothetical `PostgresWalletStorage`) translate to `StorageError` inside their impl.

#### `SqliteWalletStorage`

```rust
/// SQLite-backed wallet storage.
///
/// Wraps `zcash_client_sqlite::WalletDb` with Zally-named methods. All `WalletStorage` calls
/// route through `tokio::task::spawn_blocking`.
pub struct SqliteWalletStorage { /* ... */ }

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SqliteWalletStorageOptions {
    pub db_path: std::path::PathBuf,
    pub network: zally_core::Network,
    pub account_name: String,
}

impl SqliteWalletStorageOptions {
    /// Production-safe defaults: account_name = "primary".
    #[must_use]
    pub fn for_network(network: zally_core::Network, db_path: std::path::PathBuf) -> Self;

    /// Fast options for tests; `account_name` defaults to "primary".
    #[must_use]
    pub fn for_local_tests(db_path: std::path::PathBuf) -> Self;
}

impl SqliteWalletStorage {
    #[must_use]
    pub fn new(options: SqliteWalletStorageOptions) -> Self;
}

#[async_trait::async_trait]
impl WalletStorage for SqliteWalletStorage { /* spawn_blocking-wrapped */ }
```

Internally `SqliteWalletStorage` holds `Arc<tokio::sync::Mutex<Option<WalletDb<...>>>>`. The `Option` is `None` before `open_or_create`; `Some(db)` after. Every public method acquires the mutex, locks the inner `Option` for blocking work via a `spawn_blocking` closure, and reports a typed error if the database has not been opened.

Translation between `zally_core::AccountId` and `zcash_client_sqlite::AccountUuid` is `AccountUuid::from_uuid(account_id.as_uuid())` and `AccountId::from_uuid(account_uuid.expose_uuid())`. Both are pure functions on the inner `Uuid`. There is no persisted translation table; there is no in-memory translation cache.

#### `StorageError`

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// not_retryable: caller must call `open_or_create` first.
    #[error("wallet storage was not opened; call open_or_create first")]
    NotOpened,

    /// requires_operator: a schema mismatch requires manual intervention.
    #[error("wallet database migration failed: {reason}")]
    MigrationFailed { reason: String },

    /// retryable: transient lock contention or disk pressure may self-heal.
    #[error("sqlite error: {reason}")]
    SqliteFailed { reason: String, is_retryable: bool },

    /// not_retryable.
    #[error("account not found in wallet")]
    AccountNotFound,

    /// not_retryable: caller should use `Wallet::open` instead of `Wallet::create`.
    #[error("an account already exists in this wallet; one-account-per-wallet is the Slice 1 invariant")]
    AccountAlreadyExists,

    /// retryable: the tokio runtime may accept the task on retry.
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed { reason: String },
}

impl StorageError {
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::SqliteFailed { is_retryable, .. } => *is_retryable,
            Self::BlockingTaskFailed { .. } => true,
            Self::NotOpened
            | Self::MigrationFailed { .. }
            | Self::AccountNotFound
            | Self::AccountAlreadyExists => false,
        }
    }
}
```

The `SqliteFailed` variant is the one place posture varies by context (lock contention is retryable; "table missing" is not). It carries the field. Every other variant's posture is uniform; no field.

---

### 2.4 `zally-wallet`

#### `WalletError`

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WalletError {
    #[error("seed sealing error: {0}")]
    Sealing(#[from] zally_keys::SealingError),

    #[error("storage error: {0}")]
    Storage(#[from] zally_storage::StorageError),

    #[error("key derivation error: {0}")]
    KeyDerivation(#[from] zally_keys::KeyDerivationError),

    /// not_retryable: caller should use `Wallet::create` for first-time bootstrap.
    #[error("no sealed seed found for wallet")]
    NoSealedSeed,

    /// not_retryable: caller should use `Wallet::open` instead.
    #[error("an account already exists at this wallet location")]
    AccountAlreadyExists,

    /// not_retryable: the unsealed seed does not match any account in storage.
    #[error("no account in storage matches the unsealed seed")]
    AccountNotFound,

    /// requires_operator: configuration mismatch between caller and storage.
    #[error("network mismatch: storage={storage:?}, requested={requested:?}")]
    NetworkMismatch {
        storage: zally_core::Network,
        requested: zally_core::Network,
    },
}

impl WalletError {
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Sealing(e) => e.is_retryable(),
            Self::Storage(e) => e.is_retryable(),
            Self::KeyDerivation(e) => e.is_retryable(),
            Self::NoSealedSeed
            | Self::AccountAlreadyExists
            | Self::AccountNotFound
            | Self::NetworkMismatch { .. } => false,
        }
    }
}
```

`NetworkMismatch` has two fields (`storage`, `requested`), not three: `SeedSealing` is network-agnostic.

#### `Wallet`

```rust
/// Operator-facing wallet handle.
///
/// `Wallet` is cheap to clone; cloning shares the inner sealing and storage handles via `Arc`.
/// All async methods are cancellation-safe.
#[derive(Clone)]
pub struct Wallet { /* ... */ }

impl Wallet {
    /// Creates a new wallet.
    ///
    /// Generates a 24-word BIP-39 mnemonic, derives the seed, seals it via the provided sealing
    /// implementation, opens or creates the storage, and creates the wallet's first account at
    /// the given birthday.
    ///
    /// Returns the wallet handle, the new account's `AccountId`, and the generated `Mnemonic`.
    /// The operator must record the mnemonic out-of-band; Zally does not back it up.
    ///
    /// Returns `WalletError::AccountAlreadyExists` if the storage already contains an account.
    ///
    /// requires_operator on `AccountAlreadyExists`. retryable on transient I/O.
    pub async fn create<S, St>(
        network: zally_core::Network,
        sealing: S,
        storage: St,
        birthday: zally_core::BlockHeight,
    ) -> Result<(Self, zally_core::AccountId, zally_keys::Mnemonic), WalletError>
    where
        S: zally_keys::SeedSealing,
        St: zally_storage::WalletStorage;

    /// Opens an existing wallet.
    ///
    /// Unseals the existing seed, opens (idempotently) the storage, and looks up the account
    /// whose UFVK matches the sealed seed. Returns the wallet handle and the existing
    /// `AccountId`.
    ///
    /// Returns `WalletError::NoSealedSeed` if no sealed seed exists; callers in that case should
    /// use `Wallet::create`. Returns `WalletError::AccountNotFound` if the seed does not match
    /// any account in storage.
    ///
    /// requires_operator on `NoSealedSeed` or `AccountNotFound`. retryable on transient I/O.
    pub async fn open<S, St>(
        network: zally_core::Network,
        sealing: S,
        storage: St,
    ) -> Result<(Self, zally_core::AccountId), WalletError>
    where
        S: zally_keys::SeedSealing,
        St: zally_storage::WalletStorage;

    /// Returns the network this wallet is bound to.
    #[must_use]
    pub fn network(&self) -> zally_core::Network;

    /// Returns the runtime capability descriptor.
    ///
    /// Agents read this to feature-detect supported sealing, storage, and protocol features
    /// without pinning a Zally version.
    #[must_use]
    pub fn capabilities(&self) -> WalletCapabilities;

    /// Derives, persists, and marks-as-exposed the next available Unified Address for
    /// `account_id`. Repeated calls walk forward through diversifier indices per ZIP-316;
    /// Slice 1 inherits the upstream `WalletWrite::get_next_available_address` semantics.
    ///
    /// not_retryable on unknown account. retryable on transient I/O.
    pub async fn derive_next_address(
        &self,
        account_id: zally_core::AccountId,
    ) -> Result<zcash_keys::address::UnifiedAddress, WalletError>;
}
```

**One account per wallet invariant**: Zally v1 commits to exactly one account per wallet. `Wallet::create` produces it; `Wallet::open` looks it up. Multi-account support is a v2 concern and requires its own ADR. This invariant lets `Wallet::open` return a single `AccountId` honestly; it lets `Wallet::create` panic-free name "the account" without ambiguity; it matches PRD REQ-CORE-3 (multi-receiver, single account).

#### `WalletCapabilities`

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct WalletCapabilities {
    pub network: zally_core::Network,
    pub sealing: SealingCapability,
    pub storage: StorageCapability,
    pub features: std::collections::BTreeSet<Capability>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum SealingCapability {
    AgeFile,
    InMemory,
    #[cfg(feature = "unsafe_plaintext_seed")]
    Plaintext,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum StorageCapability {
    Sqlite,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum Capability {
    Zip316UnifiedAddresses,
}
```

Slice 1 advertises one protocol feature (`Zip316UnifiedAddresses`); Slices 2-4 add more via additive variants under `#[non_exhaustive]`.

Every public domain and capability type is serde-derive-gated under the `serde` feature (ADR-0002 Decision 10).

---

### 2.5 `zally-testkit`

#### `InMemorySealing`

```rust
/// In-memory `SeedSealing` for tests.
///
/// Holds the `SeedMaterial` in a `parking_lot::Mutex`. The `Arc<...>` backing store can be
/// shared between two `InMemorySealing` handles to simulate process restart in T1 tests.
pub struct InMemorySealing { /* ... */ }

impl InMemorySealing {
    /// New sealing with its own backing store.
    #[must_use]
    pub fn new() -> Self;

    /// New sealing that shares its backing store with `other`.
    ///
    /// Use this to simulate "open with a fresh handle to the same sealed seed" without
    /// touching the filesystem.
    #[must_use]
    pub fn shared_with(&self) -> Self;
}

impl Default for InMemorySealing {
    fn default() -> Self { Self::new() }
}
```

#### `TempWalletPath`

```rust
pub struct TempWalletPath {
    dir: tempfile::TempDir,
}

impl TempWalletPath {
    pub fn create() -> Result<Self, std::io::Error>;

    #[must_use]
    pub fn db_path(&self) -> std::path::PathBuf;

    #[must_use]
    pub fn seed_path(&self) -> std::path::PathBuf;
}
```

#### `live` module

```rust
pub const LIVE_TEST_IGNORE_REASON: &str = "live test; see CLAUDE.md §Live Node Tests";

#[must_use = "hold the returned guard for the duration of the test"]
pub fn init() -> impl Drop;

pub fn require_live() -> Result<(), LiveTestError>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LiveTestError {
    #[error("live tests are not available in Slice 1; Slice 2 adds chain connectivity")]
    NotConfigured,
}
```

---

## 3. Internal Data Flow

### `Wallet::create(network, sealing, storage, birthday)`

1. **Network validation**. `network` must equal `storage.network()`. Mismatch returns `WalletError::NetworkMismatch` immediately.
2. **Storage open**. `storage.open_or_create().await`. Idempotent.
3. **Pre-flight unseal check**. `sealing.unseal_seed().await`.
   - If `Ok(seed)`: an existing sealed seed exists. Run `storage.find_account_for_seed(&seed)`. If `Some(_)`, return `WalletError::AccountAlreadyExists`. If `None`, the wallet is in a recoverable but unexpected state (sealed seed without an account); for Slice 1 we also return `AccountAlreadyExists` (the operator should delete the orphan seal or use `Wallet::open` after first creating the account out-of-band).
   - If `Err(SealingError::NoSealedSeed)`: expected fresh-bootstrap path; proceed.
   - Other `Err(_)`: propagate.
4. **Mnemonic generation**. `Mnemonic::generate()` produces a 256-bit-entropy 24-word phrase.
5. **Seed derivation**. `SeedMaterial::from_mnemonic(&mnemonic, "")`.
6. **Seed sealing**. `sealing.seal_seed(&seed).await`.
7. **Account creation**. `storage.create_account_for_seed(&seed, birthday).await` returns `AccountId`. Under the hood this calls `WalletWrite::import_account_hd("primary", &SecretVec, zip32::AccountId::ZERO, &AccountBirthday::from_parts(ChainState::empty(birthday - 1, BlockHash::default()), None), None)` inside `spawn_blocking`. The `UnifiedSpendingKey` returned by `import_account_hd` is dropped (and zeroized) before the `spawn_blocking` closure returns.
8. **Plaintext-sealing warning**. If `wallet.capabilities().sealing == SealingCapability::Plaintext`, emit `tracing::warn!(target: "zally::wallet", event = "plaintext_seed_in_use", "wallet opened with plaintext seed sealing; never use in production")`. Fired once per `create` call.
9. **Return** `(Wallet, AccountId, Mnemonic)`.

### `Wallet::open(network, sealing, storage)`

1. **Network validation**. Same as create.
2. **Storage open**. `storage.open_or_create().await`.
3. **Seed unseal**. `sealing.unseal_seed().await`. If `Err(SealingError::NoSealedSeed)`, return `WalletError::NoSealedSeed`.
4. **Account lookup**. `storage.find_account_for_seed(&seed).await`. Under the hood: derive UFVK from seed for account index 0; call `WalletRead::get_account_for_ufvk` inside `spawn_blocking`; map the resulting `Account::id()` (an `AccountUuid`) to `AccountId`. If `None`, return `WalletError::AccountNotFound`.
5. **Plaintext-sealing warning**. As in `create` step 8.
6. **Return** `(Wallet, AccountId)`.

### `Wallet::derive_next_address(account_id)`

1. Delegates to `storage.derive_next_address(account_id).await`.
2. Inside the sqlite impl: translate `AccountId` to `AccountUuid` (identity on `Uuid`); call `WalletWrite::get_next_available_address(account_uuid, UnifiedAddressRequest::ALLOW_ALL)` inside `spawn_blocking`. If the result is `Ok(None)`, return `StorageError::AccountNotFound`. Otherwise return the `UnifiedAddress`.

---

## 4. Tests

### 4.1 T0 Unit Tests

**`zally-core`**:
- `zatoshis_max_accepted`, `zatoshis_above_max_rejected`
- `idempotency_key_valid_ascii`, `idempotency_key_empty_rejected`, `idempotency_key_non_ascii_rejected`
- `account_id_uuid_round_trip`
- `network_regtest_all_at_genesis_activates_at_height_1`

**`zally-keys`**:
- `sealing_error_retryable_match_complete`: every variant has an explicit `is_retryable()` arm (verified by `wildcard_enum_match_arm` lint + a test that constructs every variant).
- `mnemonic_generate_then_parse_round_trip`: a generated mnemonic parses back via `from_phrase`.
- `mnemonic_invalid_phrase_rejected`.
- `derive_ufvk_deterministic`: same seed + index produces the same UFVK on two calls.
- `seed_material_from_mnemonic_round_trip`.

**`zally-storage`**:
- `storage_error_retryable_match_complete`.
- `account_id_translation_is_identity_for_sqlite`: a Zally `AccountId(Uuid)` round-trips through `AccountUuid` and back unchanged.

**`zally-wallet`**:
- `wallet_error_retryable_match_complete`.
- `wallet_is_clone`: compile-time check via a `fn assert_clone<T: Clone>() {}` invocation on `Wallet`.

**`zally-testkit`**:
- `in_memory_sealing_round_trip`: seal then unseal returns identical bytes.
- `in_memory_sealing_shared_state`: two handles produced by `shared_with` see the same sealed seed.
- `temp_wallet_path_cleanup`: directory removed on drop.

### 4.2 T1 Integration Tests (in `zally-wallet/tests/integration/`)

**`tests/integration/create_then_open_round_trip.rs`**

1. `TempWalletPath::create()`; construct `AgeFileSealing` + `SqliteWalletStorage`.
2. `Wallet::create(Network::regtest_all_at_genesis(), sealing, storage, BlockHeight::from(1))` returns `(wallet, account_id, mnemonic)`.
3. Capture `ua_first = wallet.derive_next_address(account_id).await`.
4. Drop `wallet`. Build fresh `AgeFileSealing` and `SqliteWalletStorage` at the same paths.
5. `Wallet::open(...)` returns `(wallet2, account_id_2)`.
6. Assert `account_id == account_id_2`.
7. Capture `ua_second = wallet2.derive_next_address(account_id_2).await`.
8. Assert `ua_first.encode(&params) != ua_second.encode(&params)` (each call advances the diversifier index per ZIP-316 upstream semantics).
9. **Separately** verify deterministic re-derivation: do a third open of the wallet, capture the *first* call to `derive_next_address`, and assert it produces an address whose diversifier index is strictly greater than `ua_second`'s. (Per `WalletWrite::get_next_available_address`'s contract, the wallet never re-uses an exposed diversifier.)

Verifies REQ-CORE-1, REQ-CORE-2, REQ-CORE-4.

**`tests/integration/create_then_open_round_trip_in_memory.rs`**

Same shape, with `InMemorySealing::new()` then `sealing.shared_with()` to simulate process restart without touching disk.

**`tests/integration/unsafe_plaintext_seed_warns.rs`** (gated `#[cfg(feature = "unsafe_plaintext_seed")]`)

1. Install a capturing `tracing_subscriber::Layer`.
2. `Wallet::create(..., PlaintextSealing::new(path), ..., birthday).await`.
3. Drop the wallet. `Wallet::open(..., PlaintextSealing::new(path), ...).await`.
4. Assert at least two `WARN`-level events with `target = "zally::wallet"` and `event = "plaintext_seed_in_use"` (one per open).

Verifies REQ-KEYS-2.

**`tests/integration/network_mismatch_fails_closed.rs`**

1. `InMemorySealing::new()` plus `SqliteWalletStorage` configured for `Network::Mainnet`.
2. Call `Wallet::create(Network::Testnet, ...)`.
3. Assert `Err(WalletError::NetworkMismatch { storage: Network::Mainnet, requested: Network::Testnet })`.
4. Assert no database file was created (the check fails before any I/O).

**`tests/integration/create_then_create_returns_already_exists.rs`**

1. `Wallet::create(...)` once successfully.
2. Drop the wallet; re-construct fresh sealing and storage at the same paths.
3. `Wallet::create(...)` again.
4. Assert `Err(WalletError::AccountAlreadyExists)`.

**`tests/integration/open_without_seal_returns_no_sealed_seed.rs`**

1. Fresh sealing (no prior `seal_seed`) and fresh storage.
2. `Wallet::open(...)`.
3. Assert `Err(WalletError::NoSealedSeed)`.

**`tests/integration/capabilities_reports_slice_1.rs`**

1. `Wallet::create(...)` with `InMemorySealing` and `SqliteWalletStorage`.
2. `wallet.capabilities()`.
3. Assert `sealing == SealingCapability::InMemory`.
4. Assert `storage == StorageCapability::Sqlite`.
5. Assert `features.contains(&Capability::Zip316UnifiedAddresses)`.
6. Assert `network == Network::regtest_all_at_genesis()`.

Verifies REQ-AX-1 for Slice 1.

---

## 5. Example File

### `examples/open-wallet/main.rs` (in `zally-wallet`)

The example creates a wallet, derives the first Unified Address, drops the wallet, re-opens from the sealed seed, derives another address, and asserts the second address differs from the first (per ZIP-316 next-available semantics). It exemplifies:

- `Network::regtest_all_at_genesis()` constructor.
- `BlockHeight::from(1)` birthday.
- The explicit re-construction of `AgeFileSealing` and `SqliteWalletStorage` at the same paths after `drop(wallet)`.
- The `Wallet::clone()` pattern for spawning a child task that derives an address concurrently with the parent.
- The `tracing` subscriber setup using `EnvFilter::try_from_default_env()`.
- An *operator* who handles the mnemonic out-of-band by writing it to a tightly-permissioned file with a `tracing::warn!` reminder that Zally does not back it up.

`main` returns `Result<(), WalletError>`; all errors propagate with `?`; no `unwrap`, no `expect`, no `println!`, no `eprintln!`. Tracing is the terminal-display path (per ADR-0002 Decision 9).

The `#[cfg(feature = "unsafe_plaintext_seed")]` branch exercises `PlaintextSealing` and asserts (via the example's own captured tracing subscriber) that the `plaintext_seed_in_use` warning fires twice (one for create, one for open).

---

## 6. Workspace Integration

### 6.1 Root `Cargo.toml` changes

```toml
[workspace]
resolver = "3"
members = [
    "crates/zally-core",
    "crates/zally-keys",
    "crates/zally-storage",
    "crates/zally-wallet",
    "crates/zally-testkit",
]
```

Additions to `[workspace.dependencies]`:

```toml
uuid = { version = "1", features = ["v4", "serde"] }
bip0039 = { version = "0.12", features = ["all-languages"] }
parking_lot = "0.12"
zip32 = "0.2"
```

`bip0039 = "0.12"` matches the librustzcash workspace pin (`zcash_keys`'s optional `zcashd-compat` feature depends on the same version). One copy in the resolved graph, regardless of whether downstream operators enable `zcashd-compat`.

### 6.2 `deny.toml` changes

None unless `bip0039`'s license disposition needs an entry; the v0.12 release is MIT or BSD-3-Clause depending on patches. A license audit at PR time confirms.

### 6.3 `clippy.toml` changes

None.

---

## 7. Validation Checklist

| Command | Slice 1 expectation |
|---|---|
| `cargo fmt --all --check` | Format clean. |
| `cargo check --workspace --all-targets --all-features` | Type-checks including `unsafe_plaintext_seed` feature. |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | No warnings. `wildcard_enum_match_arm` and `missing_docs` enforced. |
| `cargo nextest run --profile=ci` | T0 + T1 green. |
| `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps` | Every `pub` item documented. |
| `cargo deny check` | Workspace deps audited. |
| `cargo machete` | No unused deps per crate. |

---

## 8. Resolved Open Questions

### OQ-1: Raw-bytes `SeedMaterial` constructor

**Resolved**: mnemonic-only in v1. The `keys_raw_bytes` cargo feature was dropped from Slice 1 (YAGNI per ADR-0002 and CLAUDE.md). When raw-bytes construction is needed, it lands with its own RFC.

### OQ-2: BIP-39 crate

**Resolved**: `bip0039 = "0.12"`, matching the librustzcash workspace pin. Wrapped by Zally's `Mnemonic` type so the crate choice is an implementation detail; the operator-facing return type from `Wallet::create` is `zally_keys::Mnemonic`, not the underlying crate's `Mnemonic`.

### OQ-3: Wallet lifecycle constructors

**Resolved**: two direct constructors. `Wallet::create(network, sealing, storage, birthday)` and `Wallet::open(network, sealing, storage)`. No `WalletBuilder` typestate.

### OQ-4: `WalletStorage` trait mutability and error shape

**Resolved**: `&self` everywhere; no associated `Error` type. The trait canonicalises on `StorageError`. Backends with a different native error translate inside their impl. `SqliteWalletStorage` holds `Arc<tokio::sync::Mutex<Option<WalletDb<...>>>>` internally; future backends own their concurrency.

### OQ-5: AccountId surfacing in Slice 1

**Resolved**: `Wallet::create -> (Wallet, AccountId, Mnemonic)`; `Wallet::open -> (Wallet, AccountId)`. The shape commits to one-account-per-wallet for v1; multi-account is a v2 concern with its own ADR.

---

## 9. Acceptance

1. ~~OQ-1 through OQ-5 each have a recorded decision.~~ §8.
2. The Slice 1 PR exists with code that builds against this RFC.
3. The validation gate (§7) passes locally on that branch before review.
4. Gustavo Valverde (maintainer) approves the public surface before merge.

Slice 1 implementation cites this RFC. Deviation from the RFC requires an in-PR amendment, not a silent change.

---

## 10. Architectural Refinements (revision history)

This revision applied 16 changes against the initial draft to harden patterns the later slices will copy:

| Change | Reason |
|---|---|
| `RegtestParams` duplicate dropped; `Network::Regtest(LocalNetwork)` uses the upstream type | Eliminates field-for-field duplication that would force every future upgrade (`nu7`, `z_future`) into both types. |
| `bip39 = "2"` → `bip0039 = "0.12"`; Zally `Mnemonic` wrapper added | Aligns with the librustzcash workspace pin; insulates Zally's public surface from BIP-39 crate churn. |
| `WalletStorage` associated `Error` dropped; canonicalises on `StorageError` | The associated-error pattern was clamped to a single concrete type at every call site; the abstraction was inert. |
| `network` removed from `SeedSealing` and `AgeFileSealingOptions` | Sealed seeds are network-agnostic; the field was dead weight that future implementations would have to carry. |
| `is_retryable: bool` field removed from variants with uniform posture | ADR-0002 Decision 5 refined: the field exists only where posture genuinely varies per construction site. |
| `AccountMap` dropped from `zally-storage` | The sqlite backend's `AccountId` ↔ `AccountUuid` translation is the identity function on the inner `Uuid`. Persisted-table ceremony for an identity map is overhead. |
| `Wallet::derive_next_address` semantics aligned with upstream | Each call walks forward through diversifier indices per ZIP-316; matches `WalletWrite::get_next_available_address`. |
| `Wallet::create` takes `birthday: BlockHeight` | Avoids a Slice-2 signature break; lets operators provide a real birthday at creation. |
| `Wallet::create` returns Zally-owned `Mnemonic` | No BIP-39 crate type leak in the public surface. |
| `Memo` re-exported from `zcash_protocol::memo` | Re-implementing ZIP-302 in Zally is duplication; the upstream encoding rules are canonical. |
| `serde` feature gates `Serialize`/`Deserialize` on every domain and capability type | REQ-AX-1: agents persist and compare capability descriptors. |
| `#[derive(Clone)]` on `Wallet` | Operators routinely share wallet handles across tokio tasks; `Arc<Wallet>` was an unnecessary indirection. |
| Plaintext-seed warning moved from `PlaintextSealing` to `Wallet::create`/`Wallet::open` | One warning per wallet open, not one per cryptographic operation. Slice 3+ signing operations stay quiet. |
| `#[non_exhaustive]` on every `*Options` struct | Adding a field stays additive. |
| `keys_raw_bytes` feature deleted | Reserved feature gates are YAGNI; land them with the constructor. |
| One-account-per-wallet committed for v1 | Honest constraint for `Wallet::open`'s single-`AccountId` return; multi-account is a v2 concern. |

Each item lands in the implementation PR; the PR description cites this RFC §10.
