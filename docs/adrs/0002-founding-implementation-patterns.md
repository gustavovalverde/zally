# ADR-0002: Founding Implementation Patterns

| Field | Value |
|-------|-------|
| Status | Accepted |
| Product | Zally |
| Domain | Implementation patterns shared by every crate |
| Related | [PRD-0001](../prd-0001-zally-headless-wallet-library.md), [ADR-0001](0001-workspace-crate-boundaries.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

ADR-0001 locked the crate boundary set: seven crates, with `zally-wallet` as the umbrella. [Public interfaces](../architecture/public-interfaces.md) locked naming, type conventions, the error-vocabulary outline, file conventions, and stability rules. Neither speaks to the *implementation* shape that every crate copies: how synchronous librustzcash bridges into Zally's async public surface, how trait thickness trades off against upstream churn, how the `SendOutcome` of a send surfaces broadcast-then-confirmation, how `AccountId` travels across the storage boundary, how errors carry retry posture in code (not just rustdoc), how tracing emission is structured, how options refuse to inherit test defaults in production builds, how `WalletCapabilities` grows additively, and where `examples/` sits on the production-quality scale.

The first crates Zally ships establish these patterns. Slice 2 will copy whatever Slice 1 does; Slice 3 will copy whatever Slice 2 does. Pattern drift at the founding compounds. This ADR records nine implementation patterns chosen during PRD-0001 planning, after explicit consideration of two-to-three alternatives each. The rejected alternatives are kept here so a reviewer in 2027 can see the road not taken and judge whether the chosen pattern still earns its place.

Three of the ten decisions (`WalletStorage` thickness, `AccountId` model, `SendOutcome` shape) were taken interactively during planning. Six are codified here for the first time; one of them (the async-wrap idiom) was implicit in PRD-0001 §Architectural Commitment #4 and is made explicit. The tenth (serde-derive-gating) was added during RFC-0001 review when the AX gap around `WalletCapabilities` persistence and comparison was surfaced. None require breaking any prior commitment.

## Decision

### 1. Async wrap: `tokio::task::spawn_blocking` per call

librustzcash's wallet trait surface is entirely synchronous. `WalletRead`, `WalletWrite`, `InputSource`, `WalletCommitmentTrees`, and `scan_cached_blocks` are all `fn`, not `async fn`. Zally's public surface is entirely asynchronous (PRD-0001 §Architectural Commitment #4). The bridge is `tokio::task::spawn_blocking` invoked once per public async method that touches the database, the scan loop, or any other librustzcash blocking entry point. `spawn_blocking` is the *only* place blocking code runs in Zally; every other `await` is on a non-blocking future (network I/O, channel reads, timer ticks).

Within a `spawn_blocking` body, librustzcash calls run synchronously. The body owns its database connection for the duration of the call; cross-call sharing happens through a connection pool or a per-call `WalletDb::for_path(...)` invocation, never through a `&mut` reference held across an `.await`.

**Why this:** the most common deadlock in async Rust is holding a mutex or `&mut` reference across `.await`. Making the synchronous code physically incapable of awaiting closes that class of bug. The cost is one additional thread-pool hop per call; the benefit is that the runtime cannot accidentally block.

**Alternatives considered:**

- *Long-lived blocking task driven by a channel.* A single dedicated thread holds the `WalletDb` and services requests via an `mpsc` queue. Pros: amortises the `spawn_blocking` cost across many calls, easier to reason about a single writer. Cons: serialises all reads behind one thread; adds backpressure semantics Zally would have to design; fights tokio's natural concurrency; couples observers, sync, balance, and address derivation to one queue. **Rejected** because the workload includes multiple concurrent observers, a long-running sync loop, and burst balance queries; thread-pool parallelism is the natural fit.

- *Force the upstream sync.* Rewrite enough of librustzcash to expose async methods. **Rejected** as out of scope; librustzcash is a foundation, not a fork (PRD-0001 §Architectural Commitment #2).

**How this lands:** every `pub async fn` in `zally-wallet` and `zally-storage` whose body calls librustzcash has the shape:

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

`WalletStorage` is a Zally-owned trait whose methods are named in Zally's verb vocabulary (`open_or_create`, `record_idempotent_send`, `rewind_to`, `balance_for_receiver`). The full v1 surface is ~25 methods; Slice 1 introduces only the subset Slice 1 needs. Each method's body in `SqliteWalletStorage` calls whichever librustzcash trait method the current version provides; if librustzcash 0.22 names that method `rewind_to_height` and 0.23 renames it to `rewind_to_chain_state`, the Zally signature stays stable and only the impl changes.

librustzcash trait signatures never appear in any `pub` Zally API. The four type parameters on `WalletDb<C, P, CL, R>` stay inside `SqliteWalletStorage`; operators never see them.

**Why this:** the librustzcash 0.22 → 0.23 churn is documented (15-20 breaking changes per minor version; `rewind_to_height` introduced in 0.22 and replaced in 0.23). A thin pass-through would propagate every minor bump as a Zally semver-breaking release; operators integrating Zally would either pin librustzcash through Zally (defeating Zally's value as an absorption layer) or accept a Zally bump per librustzcash bump (operationally untenable). A thick re-architecture would double the v1 wrap work and risk subtle semantic drift. Medium-thick is the working middle.

**Alternatives considered:**

- *Thin re-export.* `WalletStorage` aliases `WalletRead + WalletWrite + InputSource + WalletCommitmentTrees` with a blanket impl. Pros: ships fastest; no curation work; matches librustzcash signatures byte-for-byte. Cons: every minor librustzcash bump is breaking for Zally; four trait bounds propagate everywhere; `AccountId` types diverge across backends without translation. **Rejected** because the churn evidence makes this operationally hostile to consumers.

- *Thick re-architecture.* `WalletStorage` owns the full operator-facing wallet contract; librustzcash types are entirely hidden; storage backends can be implemented without librustzcash at all. Pros: maximum insulation; the cleanest possible public API. Cons: doubles the wrap work for Slices 1-3; risks semantic drift from upstream; loses tested abstractions. **Deferred**: revisit if librustzcash exhibits *semantic* instability (a behaviour change to an unchanged signature) that medium-thick cannot absorb. Signal to watch: a stability-driven breaking change to Zally caused by something other than a librustzcash signature change.

**How this lands:** `zally-storage` defines `WalletStorage` as an `async_trait` with Zally-named methods. `SqliteWalletStorage` implements it. Slice 1 declares the methods Slice 1 calls; the full ~25-method roadmap lives in a `// roadmap:` comment block at the top of the trait, struck through as each method lands.

### 3. `AccountId` boundary translation

`zally_core::AccountId` is a Zally-owned opaque newtype:

```rust
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct AccountId(uuid::Uuid);
```

`SqliteWalletStorage` maintains a translation table between `AccountId` and `zcash_client_sqlite::AccountUuid` internally. The translation table is private to the storage impl. An in-memory storage backend would maintain its own translation between `AccountId` and `zcash_client_memory::AccountId` (a `u32` newtype). Operators see only `AccountId` everywhere in Zally's public surface.

**Why this:** `AccountUuid` (sqlite backend) and `AccountId` (memory backend) are different types in librustzcash; they are not interchangeable. Generic-over-`WalletStorage::AccountId` propagates four trait bounds through every function. Re-exporting `AccountUuid` couples Zally's public API to the sqlite-crate's choice and breaks if `AccountUuid`'s shape ever changes (it is already `Uuid`-shaped today, but the type itself is owned by `zcash_client_sqlite`, not Zally).

**Alternatives considered:**

- *Generic over `WalletStorage::AccountId`.* Pros: no translation overhead; types match librustzcash directly. Cons: `AccountUuid` appears in operator code; serialisation varies by backend; swapping a backend is breaking; PCZT idempotency keys would carry different types across backends. **Rejected**.

- *Re-export `AccountUuid` as the canonical Zally type.* Pros: zero translation cost in the common case (sqlite). Cons: couples Zally's public surface to one librustzcash crate's type; second backend (memory, postgres, libsql) pays translation cost anyway; future `AccountUuid` shape change forces a Zally break. **Rejected**.

**How this lands:** `zally-core` exposes `AccountId(uuid::Uuid)`. `SqliteWalletStorage` translates between `AccountId` and `zcash_client_sqlite::AccountUuid` by the identity function on the inner `Uuid` (`AccountUuid::from_uuid(account_id.as_uuid())` and `AccountId::from_uuid(account_uuid.expose_uuid())`). No translation table is persisted or cached: the values are bit-for-bit identical.

A future backend whose native account-id type is not `Uuid` (e.g., `zcash_client_memory::AccountId(u32)`) owns its own translation map inside its impl. The `WalletStorage` trait surface stays `AccountId`-typed; only the backend's `pub(crate)` internals carry the per-backend translation.

PCZT, idempotency keys, observers, and any future `WalletEvent` variant all reference `AccountId` only.

### 4. `SendOutcome` split shape

`Wallet::send(...)` returns:

```rust
pub struct SendOutcome {
    pub tx_id: TxId,
    pub broadcast_at_height: BlockHeight,
    pub confirmation: Confirmation,
}

pub struct Confirmation { /* opaque */ }
impl Future for Confirmation {
    type Output = Result<ConfirmedAt, WalletError>;
}

pub struct ConfirmedAt {
    pub tx_id: TxId,
    pub at_height: BlockHeight,
}
```

The outer future returned by `send` resolves the moment the transaction is in the mempool, yielding the txid and the broadcast height. The inner `Confirmation` future resolves when the transaction is mined to the wallet's configured confirmation depth.

**Why this:** the studied operator workflow (fauzec's `dispense → poll_and_reap` pattern) records the txid into its ledger the moment broadcast succeeds, then awaits confirmation independently. A single future to confirmation would force the caller to either block their handler for minutes or detach into a poll loop; both are anti-patterns for server code. Split shape matches the natural two-step ledger model without requiring a stream state machine for the common case.

**Alternatives considered:**

- *Single future to confirmation.* Pros: simplest signature; one await; idiomatic Rust. Cons: loses sub-block-time txid visibility; long-lived futures fight reasonable timeout policies; the caller cannot record `(idempotency_key, txid)` to their durable ledger until confirmation, which defeats the idempotency contract. **Rejected**.

- *Stream of stages (`Stream<Item = SendStage>`).* Pros: most general; one type covers retry, expiry rebuild, and confirmation; uniform with `observe()` stream shape. Cons: caller writes a state machine for the common case; conflates errors-during-progress with errors-from-the-call; `Failed` variant on both the stream and the `Result`. **Deferred**: revisit if a third method appears that naturally returns staged progress and the duplication starts to bite.

**How this lands:** the inner `Confirmation` future is driven by a subscription to `WalletEvent::TransactionConfirmed`, which the scan loop emits when a confirmed-depth threshold is crossed. The subscription is set up at `send` time, so dropping the `Confirmation` (or moving it across tasks) does not lose events.

### 5. Error retry posture: rustdoc tag plus method, field only where posture varies

Every Zally error enum has `#[non_exhaustive]` and exposes `pub const fn is_retryable(&self) -> bool`. Every variant's rustdoc names its retry posture using one of the three vocabulary terms from [Public interfaces §Error vocabulary](../architecture/public-interfaces.md#error-vocabulary): `retryable`, `not_retryable`, `requires_operator`.

**Variants split into two shapes:**

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

**Why this:** the original founding pattern required `is_retryable: bool` on every retryable-ish variant, even where the posture was uniform. The field then became dead weight: every adapter author had to remember to set it to the same constant value, and every reviewer had to verify the constant. Worse, the field invited future adapters to set it inconsistently, silently violating the rustdoc contract. Reserving the field for the genuinely-context-dependent case keeps the discipline where it earns its place (lock contention vs schema mismatch) and drops it where it does not.

**Alternatives considered:**

- *Mandatory field on every variant (the original ADR draft).* Cons: dead weight on uniform variants; invites silent posture drift. **Rejected** after Slice 1 RFC review.
- *Method only, no field anywhere.* Cons: loses the context-dependent case; an adapter that wraps a sqlite error cannot distinguish "lock contention, retry will help" from "table missing, retry will not." **Rejected**.
- *Wrapper type `Retryable<E>` / `NotRetryable<E>`.* Wrong granularity; one error enum has variants in multiple posture classes. **Rejected**.

**How this lands:** `WalletError`, `SealingError`, `StorageError`, and every later boundary error (`ChainSourceError`, `SubmitterError`, `PcztError`) follow the same discipline. A T0 test in each crate verifies `is_retryable()` covers every variant (`wildcard_enum_match_arm` lint plus a test that constructs every variant in scope).

A reviewer for a new variant asks one question: "does the posture depend on context, or is it the same for every construction site?" If the answer is "depends," the variant carries the field. If the answer is "same," the variant does not.

### 6. Tracing emission: explicit calls, no `#[instrument]`

Every tracing event uses an explicit `tracing::info!` / `tracing::warn!` / `tracing::error!` call at the emission point. No `#[tracing::instrument]` macro on async functions. No structured spans on every method entry/exit.

Target naming: `zally::<crate>` where `<crate>` is the unprefixed crate name. Examples: `zally::wallet`, `zally::chain`, `zally::storage`, `zally::keys`, `zally::pczt`.

Every event has at minimum:

- `target = "zally::<crate>"`
- `event = "<snake_case_noun>"` (e.g., `"wallet_opened"`, `"chain_synced"`, `"send_broadcast"`)
- A human-readable message string at the end

Domain-specific scalar fields use `snake_case` with spine unit suffixes (`tip_height`, `broadcast_at_height`, `balance_zat`, `confirmation_depth_blocks`). Display fields are wrapped with `%` (`network = %network`); debug fields with `?` (`account_id = ?account_id`). Sensitive fields are never logged (no seed material, no USK, no plaintext memo on a payment in flight).

**Why this:** explicit emission is grep-able by event name; `#[instrument]` inflates log volume with entry/exit pairs that say nothing about progress and capture argument values whether or not they are sensitive. Pattern mirrored from Zinder (`services/zinder-ingest/src/chain_ingest.rs:990-1002`).

**Alternatives considered:**

- *`#[tracing::instrument]` on every async method.* Cons: pollutes logs with entry/exit; risks capturing sensitive arguments; obscures what an event *means*. **Rejected**.

- *No tracing in library code; let the operator instrument.* Cons: operators consistently want structured progress events (PRD-0001 §Architectural Commitment #14); making them add the events ex-post defeats the observability contract. **Rejected**.

**How this lands:** every crate's `lib.rs` documents the event vocabulary it emits in rustdoc. The error-vocabulary doc (`docs/reference/error-vocabulary.md`) is paired with an event-vocabulary doc (`docs/reference/event-vocabulary.md`, added when the first ten events have landed).

### 7. Builder discipline: no `Default` for prod/test divergent options

Options structs that carry production-vs-test divergence (sync fsync policy, retry budgets, log levels, anchor depth defaults) have no `Default` impl. Constructors are explicit:

```rust
pub struct SqliteWalletStorageOptions { /* ... */ }

impl SqliteWalletStorageOptions {
    pub const fn for_network(network: Network) -> Self { /* production-safe defaults */ }
    pub const fn for_local_tests() -> Self { /* fast, fsync=false, regtest anchor */ }
}
```

`..Default::default()` cannot accidentally inherit test-safe options in production code because there is no `Default` impl to spread.

**Why this:** prevents the production-vs-test contamination class of bug. An options struct that ships test defaults in production is the kind of mistake that survives code review (the defaults look reasonable in isolation) but fails in deployment.

Pattern mirrored from Zinder (`crates/zinder-store/src/chain_store.rs` `ChainStoreOptions`).

**Alternatives considered:**

- *`Default` plus a comment.* Comments degrade. **Rejected**.

- *A `prod_safe()` static check.* Adds runtime overhead; does not catch the bug at construction. **Rejected** in favour of the type-system enforcement.

**How this lands:** every options struct in Zally that has prod/test divergence uses this pattern. Options structs without prod/test divergence (e.g., a simple `MemoEncodingOptions { allow_arbitrary: bool }` with no environment-sensitive default) may have `Default`; the discipline applies specifically to environment-divergent options.

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
    // future: PcztV06, Zip231MemoBundlesReady, SealingAwsKms, ...
}
```

New capabilities land as additive enum variants under `#[non_exhaustive]` discipline. Never as boolean fields. Never as free-form strings.

**Why this:** PRD-0001 §REQ-AX-1 mandates a typed capability descriptor. Agents read capabilities at runtime to do feature detection without pinning a Zally version (PRD-0001 §User Story 22). Enum variants give an exhaustive match in code; the `#[non_exhaustive]` discipline means a new variant cannot break a caller's existing match arms. Boolean fields would force every consumer to know every flag and would not survive feature additions without ceremony.

**Alternatives considered:**

- *Boolean fields per capability.* Cons: every new capability is a public-struct addition; no exhaustive match; consumers cannot iterate. **Rejected**.

- *String identifiers.* Cons: no type safety; typos pass review; consumers grep instead of type-check. **Rejected**.

**How this lands:** Slice 1 ships the struct with `Network`, `SealingAgeFile`, `StorageSqlite`, `Zip316UnifiedAddresses`. Slice 2 adds `Zip317ConventionalFee` (when fees first appear), `ChainSourceZinder`. Slice 3 adds idempotency, ZIP 302/320/213/203 markers. Slice 4 adds `PcztV06`. Each capability entry has a rustdoc paragraph and an entry in `docs/reference/capability-vocabulary.md` (added with the first capability that lands).

### 9. Examples are production code

Every file under `examples/` is held to the same standard as `src/`:

- No `unwrap`, no `expect`, no `panic!`, no `eprintln!`, no `println!` except in the example's terminal-display path (which goes through `tracing::info!` where possible).
- Full rustdoc on every helper function and module-level doc on every `main.rs`.
- The `main` returns `Result<(), WalletError>` (or the appropriate boundary error) and propagates with `?`.
- All workspace lints apply; `cargo clippy --all-targets --all-features -- -D warnings` must pass on the example.
- No external shell prerequisites. The example runs from `cargo run --example <name>`; environment configuration uses the same env-var schema as production (`ZALLY_NODE__JSON_RPC_ADDR`, etc.) where applicable. T3 examples are gated by `ZALLY_TEST_LIVE=1` like T3 tests.

**Why this:** agents lift verbatim from `examples/` (PRD-0001 §REQ-AX-3). Treating examples as production-quality reference code is what turns the directory into an agent-readable spec. An example with `.unwrap()` teaches operators that `.unwrap()` is normal in production wallet code; the cost of that teaching outweighs the cost of writing the example correctly the first time.

**Alternatives considered:**

- *Examples allowed to use `unwrap` for brevity.* Cons: agents lift verbatim; brevity in the example becomes pattern in the operator's production code. **Rejected**.

- *Examples in a separate `examples-relaxed/` directory with looser lints.* Cons: complicates the public surface; consumers do not know which is canonical. **Rejected**.

**How this lands:** every example is a `[[example]]` entry in `zally-wallet/Cargo.toml` (or the appropriate consumer crate). The example is built and lint-checked as part of `cargo check --workspace --all-targets --all-features` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`, both of which are in the validation gate.

### 10. Domain and capability types are serde-derive-gated

Every domain type in `zally-core` (`Network`, `Zatoshis`, `BlockHeight`, `BranchId`, `TxId`, `AccountId`, `IdempotencyKey`, `Memo`) and every capability or event type in `zally-wallet` (`WalletCapabilities`, `SealingCapability`, `StorageCapability`, future `WalletEvent`) derives `serde::Serialize` and `serde::Deserialize` behind a `serde` cargo feature on the owning crate. The feature is off by default; operators opt in.

**Why this:** PRD §REQ-AX-1 requires agents to read, persist, and compare `WalletCapabilities` at runtime without linking Zally. Without serde, agents resort to parsing `Debug` output (fragile) or pinning to a Zally version (defeats the capability-descriptor's purpose). The same pressure applies to every domain type that crosses a process boundary: `Zatoshis` in a JSON ledger entry, `TxId` in a Kafka event, `AccountId` in an audit log.

The feature is gated rather than always-on because not every consumer wants the `serde` transitive dep tree; operators wiring Zally into a no-serde context (embedded, FFI) opt out.

**Alternatives considered:**

- *Always-on serde.* Pros: simplest mental model. Cons: forces every consumer into the serde dep tree; closes the door on no-serde consumers. **Rejected**.
- *Hand-rolled serialisation per type.* Pros: zero dep cost. Cons: re-implements serde for every type; agents cannot rely on a standard wire format. **Rejected**.
- *Separate `zally-serde` crate.* Pros: keeps the core crates lean. Cons: discovery friction; operators have to know to add the crate. **Rejected** in favour of an in-crate cargo feature.

**How this lands:** each crate that owns serde-shaped types declares a `serde` feature in its `Cargo.toml`. The crate's `Cargo.toml` lists `serde = { workspace = true, optional = true }`. Each type uses `#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]`. Slice 1 ships this for every type in `zally-core` and for `WalletCapabilities` plus its companion enums in `zally-wallet`. Slice 2 adds the same gating to `WalletEvent` when the event-stream API lands.

## Consequences

**Slice 1 PR is heavy.** Five crates land in one PR (`zally-core`, `zally-keys`, `zally-storage`, `zally-wallet`, `zally-testkit`) because ADR-0001's "no empty crates" rule means each crate enters only with real code. The PR will be substantial; the founding-pattern density is the reason. Slice 1 cites this ADR in its PR description and links each pattern to the lines that implement it.

**Subsequent slices are lighter.** Slices 2-6 inherit the patterns and extend the surface. Slice 2's PR is expected to be roughly half the size of Slice 1's: it adds one crate (`zally-chain`), copies the established patterns into it, and extends `zally-wallet` with sync, balances, and the event stream. Slice 3 adds the `Submitter` half of `zally-chain` and the send module; same shape.

**Pattern audits become a code-review checklist.** A reviewer for Slice 2+ can mechanically check:

- Does every public async method that touches librustzcash use `spawn_blocking`?
- Does every retryable error variant carry `is_retryable: bool`? Does every enum expose `is_retryable()`? Does every variant's rustdoc tag the posture?
- Does every options struct with prod/test divergence have `for_network` and `for_local_tests` (and no `Default`)?
- Does every public capability addition land as a `#[non_exhaustive]` enum variant?
- Does every new example compile under `-D warnings` with no `unwrap`?

The checklist falls out of the patterns. The reviewer does not invent it.

**Reverting an established pattern is expensive.** Once Slice 1 ships medium-thick `WalletStorage`, Slice 2's `ChainSource` will copy the medium-thick shape (Zally-named methods, librustzcash signatures hidden). Switching to thin or thick later means rewriting both. This is the cost we accept for fighting entropy: pattern decisions cluster up front, and we pay for them now to avoid paying many times later.

**The PRD-0001 Open Questions list updates.** OQ-3 (storage backend abstraction depth) is resolved by Decision 2 above. OQ-2 (mnemonic standard) is touched by Decision 3 (`AccountId` opacity) only tangentially; the specific BIP-39-vs-raw-bytes question remains open and lands in the Slice 1 RFC. OQ-5 (async runtime portability) is touched by Decision 1: this ADR commits to `tokio::task::spawn_blocking`, which makes runtime portability harder to add later. If a non-tokio operator materialises, an ADR amendment is required.

## Migration

No code exists in the workspace yet. Slice 1's PR implements all nine patterns simultaneously and cites this ADR. No migration of existing code is required.

PRD-0001's Open Questions section is amended in Slice 1's PR to mark OQ-3 as resolved by this ADR.

## Open questions

1. **Tokio-only runtime portability.** PRD-0001 OQ-5 asks whether smol or async-std should be supported. Default lean per the PRD: tokio-only for v1. This ADR commits to `tokio::task::spawn_blocking` as the async-wrap primitive; portability to other runtimes would require an `async-task` or similar abstraction Zally does not currently have. Revisit when a non-tokio operator surfaces. On revisit, the migration path is: introduce a `Runtime` trait, route every `spawn_blocking` call through it, ship `TokioRuntime` as the default impl. The public signatures do not change; only the internal call sites.

2. **Long-lived blocking task pool.** Decision 1 chooses per-call `spawn_blocking`. If observed performance under realistic load (a faucet processing 50+ claims/second, an exchange running 10+ concurrent observers) shows measurable per-call hop overhead, the migration is to swap the internal call sites to push work onto a dedicated thread; the public signatures remain unchanged. ADR amendment required at that point. Signal to watch: any single percentile (p50, p99) on a `Wallet::*` method dominated by `spawn_blocking` setup rather than the underlying librustzcash work.

3. **Examples discipline at scale.** Decision 9 holds examples to production-quality. If the example set grows past ~10 entries and routine maintenance becomes burdensome, a marked educational subset (e.g., `examples/educational/`) with relaxed lints may be warranted; the production-quality bar stays on `examples/<canonical>/`. v1 ships five examples (faucet, exchange-deposit, payment-processor, custody-with-pczt, mining-payout, per PRD-0001 §REQ-DOC-2); revisit at v2.

4. **`zally-pczt` cross-role network validation lives where?** Decision 2 establishes that librustzcash signatures stay inside Zally's wrap. The PCZT `coin_type` field is `u32` with no cross-role network check in the `pczt` crate; Zally has to close that gap. Where: in `zally-pczt`'s `Signer::new(pczt, expected_network)`, refusing construction on mismatch. This is consistent with Decision 2 but worth flagging because the gap is below the level of "trait signature" Decision 2 talks about. Lands in Slice 4. ADR amendment not required; the design will be in the Slice 4 RFC.

5. **`SeedSealing` granularity.** PRD-0001 OQ-4 asks whether USK at-rest sealing is also abstracted. Default lean per the PRD: no, USK is ephemeral. Decision 1 reinforces this: USK is derived inside a `spawn_blocking` body and zeroized before the body exits. Cache-at-rest of USK would require a separate sealing trait and is deferred. Revisit only if a use case for caching USK between operations materialises.
