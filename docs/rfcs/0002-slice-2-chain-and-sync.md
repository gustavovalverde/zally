# RFC 0002: Slice 2 — Chain Source and Wallet Sync

| Field | Value |
|---|---|
| Status | Accepted |
| Product | Zally |
| Slice | 2 |
| Related | [PRD-0001](../prd-0001-zally-headless-wallet-library.md), [ADR-0001](../adrs/0001-workspace-crate-boundaries.md), [ADR-0002](../adrs/0002-founding-implementation-patterns.md), [RFC-0001](0001-slice-1-open-wallet.md) |
| Created | 2026-05-12 |

## Summary

Slice 2 lands chain integration and the scan loop. Two crates ship: `zally-chain` (new, with `ChainSource` and `Submitter` traits plus the default `ZinderChainSource`) and the `sync` module inside `zally-wallet`. `zally-testkit` gains a `MockChainSource` because `zinder-testkit` does not currently ship a `ChainIndex` mock. The slice closes REQ-CHAIN-1, REQ-CHAIN-2, REQ-CHAIN-3, REQ-SYNC-1, REQ-SYNC-2, REQ-SYNC-3, REQ-SYNC-5, and surfaces the foundation for REQ-OBS-3 (event stream).

Slice 2 defers `LightwalletdChainSource` (REQ-CHAIN-4), retry/circuit-breaker (REQ-CHAIN-5), and `WalletMetrics` (REQ-OBS-2) to Slice 5. Sending (REQ-SPEND) lands in Slice 3 and consumes the `Submitter` trait this slice defines.

---

## 1. Crate Layout

### 1.1 `zally-chain` (new)

**Role**: chain-read plane and transaction-broadcast plane. `ChainSource` trait + `Submitter` trait + default `ZinderChainSource` implementation. Reconnect, retry, and circuit-breaker logic stay in scope for Slice 5; Slice 2 ships the trait surface and one direct-pass-through implementation.

| File | Primary export |
|---|---|
| `src/lib.rs` | crate root |
| `src/chain_source.rs` | `ChainSource` trait |
| `src/submitter.rs` | `Submitter` trait |
| `src/zinder_chain_source.rs` | `ZinderChainSource`, `ZinderChainSourceOptions` |
| `src/compact_block.rs` | `CompactBlock` re-export from `zcash_client_backend::proto` plus Zally helpers |
| `src/tree_state.rs` | `TreeState` re-export from `zcash_client_backend` |
| `src/chain_error.rs` | `ChainSourceError`, `SubmitterError` |

Cargo features:

- `serde` — gates serde derives on public types where applicable.

### 1.2 `zally-wallet::sync` (new module within `zally-wallet`)

The scan loop is orchestration of `zally-chain` + `zally-storage`; ADR-0001 explicitly keeps it inside `zally-wallet`. The module exposes one new public surface on `Wallet` and one new public stream type:

```rust
impl Wallet {
    pub async fn sync(&self, chain: &dyn ChainSource) -> Result<SyncOutcome, WalletError>;
    pub fn observe(&self) -> WalletEventStream;
    pub fn chain_tip(&self) -> Option<BlockHeight>;
}
```

| File | Primary export |
|---|---|
| `src/sync.rs` | `Wallet::sync`, `SyncOutcome`, sync loop internals |
| `src/event.rs` | `WalletEvent`, `WalletEventStream` |

### 1.3 `zally-testkit` additions

| File | Primary export |
|---|---|
| `src/mock_chain_source.rs` | `MockChainSource`, `MockChainSourceBuilder` |

---

## 2. Public Surface

### 2.1 `zally-chain::ChainSource`

```rust
#[async_trait::async_trait]
pub trait ChainSource: Send + Sync + 'static {
    /// Network this chain source is bound to. Constructors fail closed on mismatch with the
    /// wallet's network.
    fn network(&self) -> zally_core::Network;

    /// Returns the chain tip height the source currently sees. May lag the absolute network
    /// tip by the source's catch-up interval.
    async fn chain_tip(&self) -> Result<zally_core::BlockHeight, ChainSourceError>;

    /// Streams compact blocks in `[start..=end]`. Used by the scan loop to advance wallet
    /// state. Implementations may chunk; the stream item is exactly one block.
    async fn compact_blocks(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError>;

    /// Returns the canonical tree state at `block_height`. Used as the scan-loop anchor when
    /// the wallet birthday is above Sapling activation.
    async fn tree_state_at(
        &self,
        block_height: zally_core::BlockHeight,
    ) -> Result<TreeState, ChainSourceError>;

    /// Returns the subtree roots for the given pool starting at `start_index`. Slice 2 calls
    /// this once per scan-loop iteration to keep the wallet's note commitment trees in sync.
    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError>;

    /// Looks up a transaction. `ConfirmedAt { ... }` for mined transactions; `InMempool` for
    /// mempool entries; `NotFound` for either.
    async fn transaction_status(
        &self,
        tx_id: zally_core::TxId,
    ) -> Result<TransactionStatus, ChainSourceError>;

    /// Returns confirmed UTXOs for a transparent address at the current tip. Used by the sync
    /// loop to discover transparent receives at watched addresses.
    async fn transparent_utxos(
        &self,
        address: zally_core::TransparentAddress,
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError>;

    /// Subscribes to the source's chain-event stream. The stream emits a `ChainTipAdvanced`
    /// per finalized commit and a `ChainReorged` per reorg detection.
    async fn chain_events(&self) -> Result<ChainEventStream, ChainSourceError>;
}

pub type CompactBlockStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<CompactBlock, ChainSourceError>> + Send>>;

pub type ChainEventStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<ChainEvent, ChainSourceError>> + Send>>;
```

#### Supporting types

```rust
/// Inclusive block-height range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BlockHeightRange {
    pub start_height: zally_core::BlockHeight,
    pub end_height: zally_core::BlockHeight,
}

/// Shielded pool selector. Zally's vocabulary for `ShieldedProtocol`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ShieldedPool {
    Sapling,
    Orchard,
}

/// Status of a transaction known to the chain source.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TransactionStatus {
    /// Transaction is mined at the given height with the given hash.
    Confirmed {
        tx_id: zally_core::TxId,
        confirmed_at_height: zally_core::BlockHeight,
    },
    /// Transaction is in the mempool but not yet mined.
    InMempool { tx_id: zally_core::TxId },
    /// Transaction is unknown to the chain source.
    NotFound,
}

/// Chain-event variant the wallet observes via `chain_events`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChainEvent {
    /// The visible chain tip advanced over `block_range` (no reorg).
    ChainTipAdvanced {
        committed_range: BlockHeightRange,
        new_tip_height: zally_core::BlockHeight,
    },
    /// A reorg reverted `reverted_range` and committed `committed_range`.
    ChainReorged {
        reverted_range: BlockHeightRange,
        committed_range: BlockHeightRange,
        new_tip_height: zally_core::BlockHeight,
    },
}
```

`TransparentAddress` is a new domain type in `zally-core` (lands with this RFC; see §1.4 of the implementation).

#### Errors

`ChainSourceError` follows ADR-0002 Decision 5 (refined). Uniform-posture variants do not carry `is_retryable`:

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChainSourceError {
    /// retryable: the source may recover.
    #[error("chain source temporarily unavailable: {reason}")]
    Unavailable { reason: String },

    /// not_retryable: the requested height is below the source's earliest scan point.
    #[error("block height {requested_height} is below source's earliest available height {earliest_height}")]
    BlockHeightBelowFloor {
        requested_height: zally_core::BlockHeight,
        earliest_height: zally_core::BlockHeight,
    },

    /// not_retryable: the requested height is above the source's current tip.
    #[error("block height {requested_height} is above source's current tip {tip_height}")]
    BlockHeightAboveTip {
        requested_height: zally_core::BlockHeight,
        tip_height: zally_core::BlockHeight,
    },

    /// requires_operator: configuration mismatch.
    #[error("network mismatch: source={source:?}, requested={requested:?}")]
    NetworkMismatch {
        source: zally_core::Network,
        requested: zally_core::Network,
    },

    /// not_retryable.
    #[error("malformed compact block at height {block_height}: {reason}")]
    MalformedCompactBlock {
        block_height: zally_core::BlockHeight,
        reason: String,
    },

    /// retryable.
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed { reason: String },

    /// Posture varies per upstream cause; the field disambiguates.
    #[error("upstream chain-source error: {reason}")]
    UpstreamFailed { reason: String, is_retryable: bool },
}

impl ChainSourceError {
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Unavailable { .. } | Self::BlockingTaskFailed { .. } => true,
            Self::UpstreamFailed { is_retryable, .. } => *is_retryable,
            Self::BlockHeightBelowFloor { .. }
            | Self::BlockHeightAboveTip { .. }
            | Self::NetworkMismatch { .. }
            | Self::MalformedCompactBlock { .. } => false,
        }
    }
}
```

### 2.2 `zally-chain::Submitter`

```rust
#[async_trait::async_trait]
pub trait Submitter: Send + Sync + 'static {
    /// Network this submitter is bound to.
    fn network(&self) -> zally_core::Network;

    /// Submits a raw transaction. The returned `SubmitOutcome` discriminates the upstream
    /// reason; the caller decides whether to retry (mempool eviction is `retryable`, invalid
    /// encoding is `not_retryable`).
    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SubmitOutcome {
    /// Mempool accepted the transaction.
    Accepted { tx_id: zally_core::TxId },
    /// Mempool already had this transaction.
    Duplicate { tx_id: zally_core::TxId },
    /// Transaction rejected; caller cannot make it succeed by retrying.
    Rejected { reason: String },
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubmitterError {
    /// retryable.
    #[error("submitter temporarily unavailable: {reason}")]
    Unavailable { reason: String },

    /// requires_operator: configuration mismatch.
    #[error("network mismatch: submitter={submitter:?}, transaction={transaction:?}")]
    NetworkMismatch {
        submitter: zally_core::Network,
        transaction: zally_core::Network,
    },

    /// retryable.
    #[error("blocking task panicked or was cancelled: {reason}")]
    BlockingTaskFailed { reason: String },

    /// Posture varies per upstream cause.
    #[error("upstream submitter error: {reason}")]
    UpstreamFailed { reason: String, is_retryable: bool },
}
```

### 2.3 `ZinderChainSource`

```rust
/// Default `ChainSource` implementation backed by `zinder-client::ChainIndex`.
///
/// Wraps either `LocalChainIndex` (colocated; RocksDB-secondary reads) or `RemoteChainIndex`
/// (gRPC) without distinguishing them in the public API.
pub struct ZinderChainSource { /* private */ }

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ZinderChainSourceOptions {
    pub network: zally_core::Network,
    pub mode: ZinderConnectionMode,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ZinderConnectionMode {
    /// Connect via gRPC to a remote `zinder-query` endpoint.
    Remote { endpoint: String },
    /// Open a colocated `LocalChainIndex` over a Zinder canonical RocksDB.
    Local {
        primary_path: std::path::PathBuf,
        secondary_path: std::path::PathBuf,
        subscription_endpoint: Option<String>,
    },
}

impl ZinderChainSource {
    pub async fn connect(options: ZinderChainSourceOptions) -> Result<Self, ChainSourceError>;
}

#[async_trait::async_trait]
impl ChainSource for ZinderChainSource { /* ... */ }
```

`ZinderChainSource` also exposes a companion `ZinderSubmitter` constructed via `ZinderChainSource::submitter() -> impl Submitter`, which delegates `submit` to the underlying `ChainIndex::broadcast_transaction`. Slice 3's send flow takes both as arguments.

### 2.4 New domain types in `zally-core`

```rust
/// Transparent Zcash address (P2PKH or P2SH).
///
/// Carries the network as part of its identity so a function that takes `TransparentAddress`
/// can never silently mix mainnet and testnet inputs.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransparentAddress {
    network: Network,
    bytes: [u8; 21], // 1-byte type + 20-byte hash
}

impl TransparentAddress {
    pub fn from_encoded(encoded: &str, network: Network) -> Result<Self, TransparentAddressError>;
    pub fn encode(&self) -> String;
    pub fn network(&self) -> Network;
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum TransparentAddressError {
    /// not_retryable.
    #[error("transparent address decode failed: {reason}")]
    InvalidEncoding { reason: String },

    /// requires_operator: address is for a different network than expected.
    #[error("transparent address is for network {actual:?}; expected {expected:?}")]
    NetworkMismatch { expected: Network, actual: Network },
}
```

`SubtreeIndex(u32)`, `SubtreeRoot { index: SubtreeIndex, root: [u8; 32] }`, `TransparentUtxo`, and `Outpoint` are added with full rustdoc and serde gates.

### 2.5 `WalletEvent`

```rust
/// Push notification from the wallet's sync loop.
///
/// The stream is bounded; if a consumer falls behind, the oldest events are dropped and the
/// stream emits `Lagged { dropped_count }`. Consumers must handle this to remain correct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum WalletEvent {
    /// Scan progress update.
    ScanProgress {
        scanned_height: zally_core::BlockHeight,
        target_height: zally_core::BlockHeight,
    },
    /// A transaction was confirmed at `confirmed_at_height`.
    TransactionConfirmed {
        tx_id: zally_core::TxId,
        confirmed_at_height: zally_core::BlockHeight,
    },
    /// A note belonging to `account_id` was observed at `seen_at_height`.
    ReceiverObserved {
        account_id: zally_core::AccountId,
        seen_at_height: zally_core::BlockHeight,
        pool: zally_chain::ShieldedPool,
    },
    /// A reorg rolled the wallet back to `rolled_back_to_height`.
    ReorgDetected {
        rolled_back_to_height: zally_core::BlockHeight,
        new_tip_height: zally_core::BlockHeight,
    },
    /// The consumer's receive end fell behind; `dropped_count` events were skipped.
    Lagged { dropped_count: u64 },
}

pub struct WalletEventStream { /* private */ }

impl WalletEventStream {
    /// Receive the next event. `None` when the underlying broadcaster has shut down.
    pub async fn next(&mut self) -> Option<WalletEvent>;
}
```

Implementation: `tokio::sync::broadcast` channel inside `WalletInner`. `Wallet::observe()` returns a new `WalletEventStream` wrapping a receiver; channel capacity is `1024` events. Slow consumers see `Lagged` and must drain or unsubscribe.

### 2.6 `Wallet::sync`

```rust
impl Wallet {
    /// Advances the wallet from its last-scanned height to `chain.chain_tip()` by streaming
    /// compact blocks, decrypting Sapling/Orchard outputs, recording received notes, and
    /// detecting reorgs.
    ///
    /// Returns a `SyncOutcome` summarising the scan. Side effects are visible through
    /// `Wallet::observe()`'s event stream.
    ///
    /// not_retryable on `WalletError::NetworkMismatch`. retryable on transient
    /// `ChainSourceError::Unavailable`. requires_operator on storage migration mismatch.
    pub async fn sync(&self, chain: &dyn ChainSource) -> Result<SyncOutcome, WalletError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SyncOutcome {
    pub scanned_from_height: zally_core::BlockHeight,
    pub scanned_to_height: zally_core::BlockHeight,
    pub block_count: u64,
    pub reorgs_observed: u32,
}
```

Internal flow:

1. Validate `chain.network() == self.network()`. Mismatch → `WalletError::NetworkMismatch`.
2. Subscribe to `chain.chain_events()` so reorgs interrupt the scan.
3. Loop:
   - Read `wallet_tip = storage.scan_progress().last_scanned_height()`.
   - Read `chain_tip = chain.chain_tip()`.
   - If `wallet_tip >= chain_tip`, exit loop.
   - Fetch compact blocks `wallet_tip+1 ..= min(chain_tip, wallet_tip + BATCH)`.
   - Inside `spawn_blocking`: feed blocks to `zcash_client_backend::data_api::chain::scan_cached_blocks`. Persist via the existing `WalletStorage`.
   - Emit `WalletEvent::ScanProgress` after each batch.
   - On `ChainEvent::ChainReorged`, rewind storage to `committed_range.start - 1`, emit `ReorgDetected`, restart the loop.
4. Return `SyncOutcome`.

Batch size: `BATCH = 100` blocks. Operator-tunable via `SyncOptions` in Slice 5.

---

## 3. Workspace integration

### 3.1 Root `Cargo.toml`

```toml
members = [
    "crates/zally-core",
    "crates/zally-keys",
    "crates/zally-storage",
    "crates/zally-chain",        # new
    "crates/zally-wallet",
    "crates/zally-testkit",
]
```

`[workspace.dependencies]` change: replace the placeholder zero-SHA `zinder-client` git pin with a working path or commit pin. For local development the workspace uses a path dep to `../zinder/crates/zinder-client`; the path is documented in [docs/reference/zinder-client-pin.md] (added with this RFC) along with the rule for bumping the pin when zinder tags a release.

### 3.2 `deny.toml`

If `zinder-client`'s path dep flips to a git pin once Zinder tags a release, `allow-git` gains the Zinder GitHub URL.

---

## 4. Tests

### 4.1 T0 unit tests

Per crate, exhaustive error-variant `is_retryable()` matches; `TransparentAddress` round-trip encoding; `ShieldedPool` `serde` round trip.

### 4.2 T1 integration tests (`zally-wallet/tests/integration/`)

Each uses `MockChainSource` (no live node required):

- `sync_catches_up_to_tip.rs` — fabricated 100-block chain; assert `SyncOutcome.scanned_to_height == 100`.
- `sync_reorg_rollback.rs` — fabricated chain advances to height 50, reorgs at 40; assert wallet rolls back to 39, `WalletEvent::ReorgDetected` fires.
- `observe_receiver.rs` — fabricated block contains a shielded output to the wallet's UA; assert `WalletEvent::ReceiverObserved` fires.
- `network_mismatch_at_sync.rs` — `Wallet::sync` with a `MockChainSource` on a different network returns `WalletError::NetworkMismatch` immediately.

### 4.3 T3 live tests (`zally-wallet/tests/live/`)

Gated by `#[ignore = LIVE_TEST_IGNORE_REASON]` and `require_live()`. Slice 2 adds:

- `live_sync_against_zinder_regtest.rs` — boots Zinder against a local `zebrad --regtest` (or equivalent), syncs Zally, asserts `chain_tip()` advances.

The test enforces the same env-var contract as Zinder (`ZALLY_TEST_LIVE=1`, `ZALLY_NETWORK=regtest`, `ZALLY_NODE__*`). The test is `#[ignore]` in the default profile and runs under `cargo nextest run --profile=ci-live --run-ignored=all`. CI-live is operator-driven; this slice does not block on its execution.

---

## 5. Local-node infrastructure

`zcashd`, `zebrad`, and a running Zinder process are all absent from the default development environment as of this RFC's drafting. The Slice 2 deliverable does not require any of them to land: all T0 and T1 tests run against `MockChainSource`. The T3 live test plan is documented but not part of the validation gate. Bringing up the local infrastructure is tracked as a separate operator task (see issue list in §10).

---

## 6. Validation gate

Same commands as RFC-0001 §7. Slice 2 adds:

- `cargo machete` over six crates instead of five.
- `cargo deny check` over the new `zinder-client` path dep.

---

## 7. Capability advertisement

`WalletCapabilities::features` gains:

- `ChainSourceZinder` — Zinder is the wallet's chain plane.
- `SyncIncremental` — `Wallet::sync` is available.
- `EventStream` — `Wallet::observe` is available.

Slice 1's `Zip316UnifiedAddresses` stays; Slice 2 does not remove or rename any prior capability variant.

---

## 8. Open questions

### OQ-1: `at_epoch` exposure

The Zinder `ChainIndex` exposes `at_epoch: Option<ChainEpoch>` on every read. Slice 2's `ChainSource` hides it: the wrapper sets `None` on every call, so reads resolve to the source's currently visible epoch. This is correct for the scan loop (each batch reads against a fresh epoch) but means a long-running consumer cannot pin a cross-call snapshot.

Decision lean: hide for v1; expose via `ChainSource::pin_epoch()` if a v2 consumer surfaces a real need.

### OQ-2: `chain_events` consumed by sync or by observers?

The natural design has `Wallet::sync` subscribe to `chain.chain_events()` to react to reorgs. But `Wallet::observe()` (the wallet event stream) is also consumer-facing. Two options:

- (a) `sync` subscribes; reorg events are republished as `WalletEvent::ReorgDetected`.
- (b) Multiplex `chain_events` to both `sync` and `observe`.

Decision lean: (a). The `WalletEvent` stream is wallet-semantic; the `ChainEvent` stream is chain-semantic. Reorgs translate one to the other inside the sync loop.

### OQ-3: Confirmation depth policy

REQ-SYNC-4 mandates configurable depth per receiver purpose with the ZIP-213 100-block coinbase rule. Slice 2 ships a single global default depth of 1 confirmation; per-receiver-purpose configuration lands in Slice 3 with the receiver-purpose vocabulary that the spend flow needs.

---

## 9. Acceptance

1. Five resolved open questions recorded here.
2. Implementation builds against this RFC; T0 + T1 tests pass under the validation gate.
3. Slice 2 PR cites RFC-0002.

---

## 10. Follow-up issues

- Bring up local regtest + Zinder for T3 live testing.
- Tag a Zinder release commit so `zinder-client` can move from path-dep to git-pin.
- Add `LightwalletdChainSource` (REQ-CHAIN-4) — defer to Slice 5.
- Add retry/circuit-breaker layer wrapping `ChainSource` (REQ-CHAIN-5) — defer to Slice 5.
