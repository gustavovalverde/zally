# ADR-0002: Implementation Patterns

| Field | Value |
|-------|-------|
| Status | Accepted |
| Product | Zally |
| Domain | Implementation patterns shared by every crate |
| Related | [ADR-0001](0001-workspace-crate-boundaries.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

ADR-0001 locked the crate boundary set. [Public interfaces](../architecture/public-interfaces.md) locked naming, type conventions, the error-vocabulary outline, file conventions, and stability rules. Neither speaks to the *implementation* shape every crate copies: how synchronous librustzcash bridges into Zally's async public surface, how trait thickness trades off against upstream churn, how `SendOutcome` surfaces broadcast-then-confirmation, how `AccountId` travels across the storage boundary, how errors carry retry posture in code (not just rustdoc), how tracing emission is structured, how options refuse to inherit test defaults in production builds, how `WalletCapabilities` grows additively, and where `examples/` sits on the production-quality scale.

This ADR records ten implementation patterns. The rejected alternatives are kept so a future reviewer can see the road not taken and judge whether the chosen pattern still earns its place.

## Decision

### 1. Async wrap: `tokio::task::spawn_blocking` per call

librustzcash's wallet trait surface is entirely synchronous. `WalletRead`, `WalletWrite`, `InputSource`, `WalletCommitmentTrees`, and `scan_cached_blocks` are all `fn`, not `async fn`. Zally's public surface is entirely asynchronous. The bridge is `tokio::task::spawn_blocking` invoked once per public async method that touches the database, the scan loop, or any other librustzcash blocking entry point. `spawn_blocking` is the *only* place blocking code runs in Zally; every other `await` is on a non-blocking future (network I/O, channel reads, timer ticks).

Within a `spawn_blocking` body, librustzcash calls run synchronously. The body owns its database connection for the duration of the call; cross-call sharing happens through a connection pool or a per-call `WalletDb::for_path(...)` invocation, never through a `&mut` reference held across an `.await`.

**Why this:** the most common deadlock in async Rust is holding a mutex or `&mut` reference across `.await`. Making the synchronous code physically incapable of awaiting closes that class of bug. The cost is one additional thread-pool hop per call; the benefit is that the runtime cannot accidentally block.

**Alternatives considered:**

- *Long-lived blocking task driven by a channel.* A single dedicated thread holds the `WalletDb` and services requests via an `mpsc` queue. Pros: amortises the `spawn_blocking` cost across many calls, easier to reason about a single writer. Cons: serialises all reads behind one thread; adds backpressure semantics Zally would have to design; fights tokio's natural concurrency; couples observers, sync, balance, and address derivation to one queue. **Rejected** because the workload includes multiple concurrent observers, a long-running sync loop, and burst balance queries; thread-pool parallelism is the natural fit.

- *Force the upstream sync.* Rewrite enough of librustzcash to expose async methods. **Rejected** as out of scope; librustzcash is a foundation, not a fork.

**Pattern:** every `pub async fn` in `zally-wallet` and `zally-storage` whose body calls librustzcash has the shape:

```rust
pub async fn balance_for_account(&self, id: AccountId) -> Result<BalanceSnapshot, WalletError> {
    let storage = self.storage.clone();
    tokio::task::spawn_blocking(move || storage.balance_for_account_blocking(id))
        .await
        .map_err(WalletError::from_join)?
}
```

The `*_blocking` private method holds the sync librustzcash logic. The public method holds the async wrap. The split is mechanical and reviewable.

### 2. `WalletStorage` thickness: medium

`WalletStorage` is a Zally-owned trait whose methods are named in Zally's verb vocabulary (`open_or_create`, `record_idempotent_submission`, `truncate_to_height`, `propose_payment`). Each method's body in `SqliteWalletStorage` calls whichever librustzcash trait method the current version provides; if librustzcash 0.22 names that method `rewind_to_height` and 0.23 renames it to `rewind_to_chain_state`, the Zally signature stays stable and only the impl changes.

librustzcash trait signatures never appear in any `pub` Zally API. The four type parameters on `WalletDb<C, P, CL, R>` stay inside `SqliteWalletStorage`; operators never see them.

**Why this:** librustzcash minor-version churn is documented (15-20 breaking changes per minor version). A thin pass-through would propagate every minor bump as a Zally semver-breaking release. A thick re-architecture would double the wrap work and risk subtle semantic drift. Medium-thick is the working middle.

**Alternatives considered:**

- *Thin re-export.* `WalletStorage` aliases `WalletRead + WalletWrite + InputSource + WalletCommitmentTrees` with a blanket impl. Pros: ships fastest; matches librustzcash signatures byte-for-byte. Cons: every minor librustzcash bump is breaking for Zally; four trait bounds propagate everywhere; `AccountId` types diverge across backends without translation. **Rejected** because the churn evidence makes this operationally hostile to consumers.

- *Thick re-architecture.* `WalletStorage` owns the full operator-facing wallet contract; librustzcash types are entirely hidden; storage backends can be implemented without librustzcash at all. Pros: maximum insulation; the cleanest possible public API. Cons: doubles the wrap work; risks semantic drift from upstream; loses tested abstractions. **Deferred**: revisit if librustzcash exhibits *semantic* instability (a behaviour change to an unchanged signature) that medium-thick cannot absorb.

### 3. `AccountId` boundary translation

`zally_core::AccountId` is a Zally-owned opaque newtype:

```rust
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct AccountId(uuid::Uuid);
```

`SqliteWalletStorage` maintains a translation between `AccountId` and `zcash_client_sqlite::AccountUuid` internally. An in-memory storage backend would maintain its own translation between `AccountId` and `zcash_client_memory::AccountId` (a `u32` newtype). Operators see only `AccountId` everywhere in Zally's public surface.

**Why this:** `AccountUuid` (sqlite backend) and `AccountId` (memory backend) are different types in librustzcash; they are not interchangeable. Generic-over-`WalletStorage::AccountId` propagates four trait bounds through every function. Re-exporting `AccountUuid` couples Zally's public API to the sqlite-crate's choice and breaks if `AccountUuid`'s shape ever changes.

**Alternatives considered:**

- *Generic over `WalletStorage::AccountId`.* Pros: no translation overhead; types match librustzcash directly. Cons: `AccountUuid` appears in operator code; serialisation varies by backend; swapping a backend is breaking; PCZT idempotency keys would carry different types across backends. **Rejected**.

- *Re-export `AccountUuid` as the canonical Zally type.* Pros: zero translation cost in the common case (sqlite). Cons: couples Zally's public surface to one librustzcash crate's type; second backend (memory, postgres, libsql) pays translation cost anyway; future `AccountUuid` shape change forces a Zally break. **Rejected**.

**Pattern:** `zally-core` exposes `AccountId(uuid::Uuid)`. `SqliteWalletStorage` translates between `AccountId` and `zcash_client_sqlite::AccountUuid` by the identity function on the inner `Uuid`. No translation table is persisted or cached: the values are bit-for-bit identical.

A backend whose native account-id type is not `Uuid` (for example `zcash_client_memory::AccountId(u32)`) owns its own translation map inside its impl. The `WalletStorage` trait surface stays `AccountId`-typed; only the backend's `pub(crate)` internals carry the per-backend translation.

PCZT, idempotency keys, observers, and any future `WalletEvent` variant all reference `AccountId` only.

### 4. `SendOutcome` shape

`Wallet::send_payment(...)` returns:

```rust
pub struct SendOutcome {
    pub tx_id: TxId,
    pub broadcast_at_height: BlockHeight,
}
```

The future returned by `send_payment` resolves the moment the transaction is in the mempool, yielding the txid and the broadcast height. Confirmation is observed separately through `Wallet::observe()`, which emits `WalletEvent::TransactionConfirmed { tx_id, .. }` when the configured confirmation depth is crossed.

**Why this:** the studied operator workflow (record the txid into the operator's ledger at broadcast time, then poll or observe for confirmation) needs sub-block-time txid visibility. A single future to confirmation would force the caller to either block their handler for minutes or detach into a poll loop; both are anti-patterns for server code. Splitting broadcast acknowledgement from confirmation matches the natural two-step ledger model without requiring a stream state machine for the common case.

**Alternatives considered:**

- *Single future to confirmation.* Pros: simplest signature; one await; idiomatic Rust. Cons: loses sub-block-time txid visibility; long-lived futures fight reasonable timeout policies; the caller cannot record `(idempotency_key, txid)` to their durable ledger until confirmation, which defeats the idempotency contract. **Rejected**.

- *Stream of stages (`Stream<Item = SendStage>`).* Pros: most general; one type covers retry, expiry rebuild, and confirmation; uniform with `observe()` stream shape. Cons: caller writes a state machine for the common case; conflates errors-during-progress with errors-from-the-call. **Deferred**: revisit if a third method appears that naturally returns staged progress.

### 5. Error retry posture: rustdoc tag plus method, field only where posture varies

Every Zally error enum has `#[non_exhaustive]` and exposes `pub const fn is_retryable(&self) -> bool`. Every variant's rustdoc names its retry posture using one of the three vocabulary terms from [Public interfaces §Error vocabulary](../architecture/public-interfaces.md#error-vocabulary): `retryable`, `not_retryable`, `requires_operator`.

Variants split into two shapes:

- **Uniform posture** (the common case). All instances of this variant share the same retry posture regardless of construction site. No `is_retryable: bool` field. The `is_retryable()` method's match arm encodes the posture as a constant.

```rust
#[error("seed file read failed: {reason}")]
ReadFailed { reason: String },  // uniform: retryable

#[error("age decryption failed: {reason}")]
DecryptionFailed { reason: String },  // uniform: requires_operator
```

- **Context-dependent posture** (the rarer case). Different construction sites of the same variant carry different retry postures. The variant carries `is_retryable: bool`.

```rust
#[error("sqlite error: {reason}")]
SqliteFailed { reason: String, is_retryable: bool },  // lock contention is retryable;
                                                       // missing table is not
```

The `is_retryable()` method returns the field for context-dependent variants and the constant for uniform variants:

```rust
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

**Why this:** a mandatory `is_retryable: bool` on every retryable-ish variant becomes dead weight: every adapter author has to remember to set it to the same constant, and every reviewer has to verify the constant. Worse, the field invites future adapters to set it inconsistently, silently violating the rustdoc contract. Reserving the field for the genuinely-context-dependent case keeps the discipline where it earns its place (lock contention vs schema mismatch) and drops it where it does not.

**Alternatives considered:**

- *Mandatory field on every variant.* Cons: dead weight on uniform variants; invites silent posture drift. **Rejected**.
- *Method only, no field anywhere.* Cons: loses the context-dependent case; an adapter that wraps a sqlite error cannot distinguish "lock contention, retry will help" from "table missing, retry will not." **Rejected**.
- *Wrapper type `Retryable<E>` / `NotRetryable<E>`.* Wrong granularity; one error enum has variants in multiple posture classes. **Rejected**.

**Pattern:** `WalletError`, `SealingError`, `StorageError`, `ChainSourceError`, `SubmitterError`, `PcztError` follow the same discipline. A T0 test in each crate verifies `is_retryable()` covers every variant.

A reviewer for a new variant asks one question: does the posture depend on context, or is it the same for every construction site? If the answer is "depends," the variant carries the field. If the answer is "same," the variant does not.

### 6. Tracing emission: explicit calls, no `#[instrument]`

Every tracing event uses an explicit `tracing::info!` / `tracing::warn!` / `tracing::error!` call at the emission point. No `#[tracing::instrument]` macro on async functions. No structured spans on every method entry/exit.

Target naming: `zally::<crate>` where `<crate>` is the unprefixed crate name. Examples: `zally::wallet`, `zally::chain`, `zally::storage`, `zally::keys`, `zally::pczt`.

Every event has at minimum:

- `target = "zally::<crate>"`
- `event = "<snake_case_noun>"` (for example `"wallet_opened"`, `"chain_synced"`, `"send_broadcast"`)
- A human-readable message string at the end

Domain-specific scalar fields use `snake_case` with spine unit suffixes (`tip_height`, `broadcast_at_height`, `balance_zat`, `confirmation_depth_blocks`). Display fields are wrapped with `%`; debug fields with `?`. Sensitive fields are never logged (no seed material, no USK, no plaintext memo on a payment in flight).

**Why this:** explicit emission is grep-able by event name; `#[instrument]` inflates log volume with entry/exit pairs that say nothing about progress and capture argument values whether or not they are sensitive.

**Alternatives considered:**

- *`#[tracing::instrument]` on every async method.* Cons: pollutes logs with entry/exit; risks capturing sensitive arguments; obscures what an event *means*. **Rejected**.
- *No tracing in library code; let the operator instrument.* Cons: operators consistently want structured progress events; making them add the events ex-post defeats the observability contract. **Rejected**.

### 7. Builder discipline: network-explicit options

Options structs that bind to chain state take the network explicitly. Tests use the same
constructor as production code so hidden regtest defaults cannot drift away from the live stack:

```rust
pub struct SqliteWalletStorageOptions { /* ... */ }

impl SqliteWalletStorageOptions {
    pub fn for_network(network: Network, db_path: PathBuf) -> Self { /* ... */ }
}
```

`Network::regtest()` is the local Zebra/Zinder topology. Custom regtest topologies construct
`Network::Regtest(LocalNetwork)` directly from the node's activation table.

**Why this:** prevents the production-vs-test contamination class of bug. An options struct that
silently chooses a regtest network in tests can make the runtime constructor look simpler while
teaching callers the wrong habit.

**Alternatives considered:**

- *`Default` plus a comment.* Comments degrade. **Rejected**.
- *A `prod_safe()` static check.* Adds runtime overhead; does not catch the bug at construction. **Rejected** in favour of the type-system enforcement.

**Pattern:** every options struct in Zally with prod/test divergence uses this pattern. Options structs without prod/test divergence (for example a simple `MemoEncodingOptions { allow_arbitrary: bool }` with no environment-sensitive default) may have `Default`; the discipline applies specifically to environment-divergent options.

### 8. `WalletCapabilities` additive enum growth

`Wallet::capabilities() -> WalletCapabilities` returns a typed struct whose features are an enum-set:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletCapabilities {
    network: Network,
    features: BTreeSet<Capability>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum Capability {
    Zip316UnifiedAddresses,
    Zip317ConventionalFee,
    SealingAgeFile,
    StorageSqlite,
    // new capabilities land as additive variants
}
```

New capabilities land as additive enum variants under `#[non_exhaustive]` discipline. Never as boolean fields. Never as free-form strings.

**Why this:** agents read capabilities at runtime to do feature detection without pinning a Zally version. Enum variants give an exhaustive match in code; the `#[non_exhaustive]` discipline means a new variant cannot break a caller's existing match arms. Boolean fields would force every consumer to know every flag and would not survive feature additions without ceremony.

**Alternatives considered:**

- *Boolean fields per capability.* Cons: every new capability is a public-struct addition; no exhaustive match; consumers cannot iterate. **Rejected**.
- *String identifiers.* Cons: no type safety; typos pass review; consumers grep instead of type-check. **Rejected**.

### 9. Examples are production code

Every file under `examples/` is held to the same standard as `src/`:

- No `unwrap`, no `expect`, no `panic!`, no `eprintln!`, no `println!` except in the example's terminal-display path (which goes through `tracing::info!` where possible).
- Full rustdoc on every helper function and module-level doc on every `main.rs`.
- The `main` returns `Result<(), WalletError>` (or the appropriate boundary error) and propagates with `?`.
- All workspace lints apply; `cargo clippy --all-targets --all-features -- -D warnings` must pass on the example.
- No external shell prerequisites. The example runs from `cargo run --example <name>`; environment configuration uses the same env-var schema as production (`ZALLY_NODE__JSON_RPC_ADDR`, and so on) where applicable.

**Why this:** agents lift verbatim from `examples/`. Treating examples as production-quality reference code is what turns the directory into an agent-readable spec. An example with `.unwrap()` teaches operators that `.unwrap()` is normal in production wallet code; the cost of that teaching outweighs the cost of writing the example correctly the first time.

**Alternatives considered:**

- *Examples allowed to use `unwrap` for brevity.* Cons: agents lift verbatim; brevity in the example becomes pattern in the operator's production code. **Rejected**.
- *Examples in a separate `examples-relaxed/` directory with looser lints.* Cons: complicates the public surface; consumers do not know which is canonical. **Rejected**.

### 10. Domain and capability types are serde-derive-gated

Every domain type in `zally-core` (`Network`, `Zatoshis`, `BlockHeight`, `BranchId`, `TxId`, `AccountId`, `IdempotencyKey`, `Memo`) and every capability or event type in `zally-wallet` (`WalletCapabilities`, `SealingCapability`, `StorageCapability`, `WalletEvent`) derives `serde::Serialize` and `serde::Deserialize` behind a `serde` cargo feature on the owning crate. The feature is off by default; operators opt in.

**Why this:** agents read, persist, and compare `WalletCapabilities` at runtime without linking Zally. Without serde, agents resort to parsing `Debug` output (fragile) or pinning to a Zally version (defeats the capability-descriptor's purpose). The same pressure applies to every domain type that crosses a process boundary: `Zatoshis` in a JSON ledger entry, `TxId` in a Kafka event, `AccountId` in an audit log.

The feature is gated rather than always-on because not every consumer wants the `serde` transitive dep tree; operators wiring Zally into a no-serde context (embedded, FFI) opt out.

**Alternatives considered:**

- *Always-on serde.* Pros: simplest mental model. Cons: forces every consumer into the serde dep tree; closes the door on no-serde consumers. **Rejected**.
- *Hand-rolled serialisation per type.* Pros: zero dep cost. Cons: re-implements serde for every type; agents cannot rely on a standard wire format. **Rejected**.
- *Separate `zally-serde` crate.* Pros: keeps the core crates lean. Cons: discovery friction; operators have to know to add the crate. **Rejected** in favour of an in-crate cargo feature.

**Pattern:** each crate that owns serde-shaped types declares a `serde` feature in its `Cargo.toml`. The crate's `Cargo.toml` lists `serde = { workspace = true, optional = true }`. Each type uses `#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]`.

## Consequences

**Pattern audits become a code-review checklist.** A reviewer for a new crate or new method can mechanically check:

- Does every public async method that touches librustzcash use `spawn_blocking`?
- Does every retryable error variant carry `is_retryable: bool`? Does every enum expose `is_retryable()`? Does every variant's rustdoc tag the posture?
- Does every options struct that binds to chain state require an explicit `Network`?
- Does every public capability addition land as a `#[non_exhaustive]` enum variant?
- Does every new example compile under `-D warnings` with no `unwrap`?

The checklist falls out of the patterns. The reviewer does not invent it.

**Reverting an established pattern is expensive.** Switching `WalletStorage` from medium-thick to thin or thick would mean rewriting every storage method. This is the cost we accept for fighting entropy: pattern decisions cluster, and we pay for them once to avoid paying many times later.

## Open questions

1. **Tokio-only runtime portability.** This ADR commits to `tokio::task::spawn_blocking` as the async-wrap primitive; portability to other runtimes would require an `async-task` or similar abstraction Zally does not currently have. Revisit when a non-tokio operator surfaces. On revisit, the migration path is: introduce a `Runtime` trait, route every `spawn_blocking` call through it, ship `TokioRuntime` as the default impl. The public signatures do not change; only the internal call sites.

2. **Long-lived blocking task pool.** Decision 1 chooses per-call `spawn_blocking`. If observed performance under realistic load shows measurable per-call hop overhead, the migration is to swap the internal call sites to push work onto a dedicated thread; the public signatures remain unchanged. ADR amendment required at that point. Signal to watch: any single percentile (p50, p99) on a `Wallet::*` method dominated by `spawn_blocking` setup rather than the underlying librustzcash work.

3. **Examples discipline at scale.** Decision 9 holds examples to production-quality. If the example set grows past ~10 entries and routine maintenance becomes burdensome, a marked educational subset (`examples/educational/`) with relaxed lints may be warranted; the production-quality bar stays on `examples/<canonical>/`.

4. **`SeedSealing` granularity.** USK is ephemeral (Decision 1: derived inside a `spawn_blocking` body and zeroized before the body exits). Cache-at-rest of USK would require a separate sealing trait and is deferred. Revisit only if a use case for caching USK between operations materialises.
