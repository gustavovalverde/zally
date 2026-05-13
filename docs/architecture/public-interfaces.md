# Public Interfaces

The vocabulary spine. Every public type, function, error variant, trait method, config field, env var, file name, and directory in Zally defers to the conventions in this document. The conventions exist so that an operator integrating Zally, a contributor extending it, or an LLM agent navigating it can predict the right name and the right shape from context alone.

This is the first document to read before writing any public Rust type. Drift here is expensive to revert; consistency here pays back at every read.

## Naming rules

### Forbidden roots, anywhere in any identifier

These words carry no bounded context. If a name needs them to be intelligible, the boundary is not understood.

- `utils`, `helpers`, `common`, `shared`, `manager`, `handler`, `processor`
- `data`, `info`, `item`, `result`, `stuff`, `thing`, `tmp`, `value`, `payload`
- `obj`, `foo`, `bar`

As suffixes on type names: `*Service`, `*Server`, `*Api`, `*Manager`, `*Processor`, `*Helper`, `*Util`, `*Data`, `*Info`. Kept: `*Error` (standard trait extension), `*Strategy` (rule bundle, e.g., `FeeStrategy`), `*Source` (read seam, e.g., `ChainSource`), `*Storage` (write seam, e.g., `WalletStorage`).

### Required suffixes on numeric and lifecycle identifiers

A bare number is a lie. Operators and agents must not have to read documentation to know whether `60` means seconds, milliseconds, blocks, or something else.

- Duration: `_ms`, `_seconds`, `_minutes`, `_hours`, `_blocks`, `_height`. Never bare `timeout`, `delay`, `interval`.
- Money: `_zat` for integer zatoshis, `_zec` for decimal-string ZEC. Never bare `amount`.
- Booleans: `is_*`, `has_*`, `can_*`. Affirmative only. Never `is_not_ready`, never `disabled` (use `is_disabled` only if the call sites read negatively).
- Counts: `_count` suffix on integer counts. Never bare `total`.
- Bytes: `_bytes` suffix when the unit is bytes. Never bare `size`.

### Network-tagged everywhere

The most catastrophic class of wallet bug is signing a testnet transaction with mainnet keys (or vice versa). Make the bug unrepresentable.

- Every public type that names an address, key, balance, or transaction carries a `Network` value or is generic over a `NetworkSpec` parameter.
- A function that takes an address but not a network is a review-blocking smell.
- Constructors fail closed on mismatch. `Wallet::open(seed, Network::Testnet)` opens a testnet wallet; calling its methods with a mainnet address is a typed error at compile time where possible, at runtime otherwise.

### Verbs from a single vocabulary

Mixed vocabulary (`fetch`/`retrieve`/`get` for the same operation) burns reader working memory. Zally pins one verb per operation.

| Verb | Meaning |
|---|---|
| `get_*` | synchronous or cached read with no I/O |
| `find_*` | lookup that may return `None` |
| `compute_*` | pure deterministic calculation, no I/O |
| `derive_*` | protocol-level derivation (cryptographic, ZIP-32 keys, ZIP-244 digests) |
| `resolve_*` | polymorphic dispatch returning one authoritative value |
| `open_*` / `close_*` | wallet lifecycle |
| `create_*` | allocate or insert a new entity |
| `propose_*` | build a `Proposal` without signing (librustzcash vocabulary) |
| `sign_*` | apply signature material to a `Proposal` or `Pczt` |
| `submit_*` | broadcast a signed transaction to the network |
| `send_*` | high-level: propose + sign + submit in one call |
| `seal_*` / `unseal_*` | apply or remove at-rest encryption to seed material |
| `observe_*` | subscribe to events (`WalletEvent` stream) |
| `sync_*` | catch up wallet state with chain |
| `export_*` | serialise material for external consumption (UFVK, PCZT) |

Forbidden verbs for domain operations: `handle_*`, `process_*`, `manage_*`, `do_*`, `perform_*`, `execute_*`. They name an action without naming *which* action. (`handle_*` is permitted only in UI event callbacks, which Zally has none of.)

### No temporal or implementation drift

A name must survive a change of its implementation.

- Banned patterns: `new_x`, `x2`, `legacy_x`, `x_old`, `x_final`, `x_real`, `x_actual`, `x_improved`. These are migration scars.
- Banned implementation leaks: `sqlite_storage` (the implementation is `WalletStorage` with `SqliteWalletStorage` as one impl, not `SqliteStorage`), `zinder_chain` (`ChainSource` with `ZinderChainSource` as one impl).
- Protocol names are domain, not implementation, and stay: `pczt`, `frost`, `zip32`, `zip316`, `zip321`, `zip317`, `zip320`, `tex`. These appear verbatim in identifiers.

## Type conventions

### Domain types

| Type | Crate | Role |
|---|---|---|
| `Network` | `zally-core` | Enum: `Mainnet`, `Testnet`, `Regtest`. Every public type carrying chain state is tagged. |
| `Zatoshis` | `zally-core` | `u64` non-negative newtype for transaction amounts. Never bare `u64`. |
| `BlockHeight` | `zally-core` | `u32` newtype. Never bare `u32` for height. |
| `BranchId` | `zally-core` | ZIP-200 branch identifier; required at transaction construction time. |
| `TxId` | `zally-core` | Non-malleable txid per ZIP-244. Never bare `[u8; 32]`. |
| `AccountId` | `zally-core` | Opaque identifier for an account within a wallet. |
| `ReceiverPurpose` | `zally-core` | Enum naming each receiver's role (`Mining`, `Donations`, `HotDispense`, `ColdReserve`, `Custom(String)`). |
| `IdempotencyKey` | `zally-core` | Caller-supplied identifier for send-idempotency. `AsRef<str>` newtype. |
| `Memo` | `zally-core` | ZIP-302 memo wrapper. Refuses construction over 512 bytes. |
| `Wallet` | `zally-wallet` | The operator-facing handle. Async API. |
| `WalletStorage` | `zally-storage` | Trait abstraction over librustzcash's `WalletRead` + `WalletWrite`. |
| `ChainSource` | `zally-chain` | Trait for compact-block reads, tree state, transparent UTXO lookups. |
| `Submitter` | `zally-chain` | Trait for transaction broadcast. |
| `SeedSealing` | `zally-keys` | Trait for at-rest seed encryption. |
| `WalletCapabilities` | `zally-wallet` | Runtime descriptor of supported features (ZIP coverage, PCZT version, sealing impl). |

### Discriminated unions

For state-machine types, the discriminant field is `status:` (lifecycle) or `kind:` (variant):

```rust
pub enum SyncStatus {
    Starting,
    Catching { current: BlockHeight, target: BlockHeight },
    AtTip { since: SystemTime },
    Reorg { rolled_back_to: BlockHeight },
}

pub enum ReceiverKind {
    Transparent,
    Sapling,
    Orchard,
    Unified,
}
```

### Trait shape

Traits exposing pluggable boundaries (`ChainSource`, `Submitter`, `WalletStorage`, `SeedSealing`) follow this shape:

- Trait-associated `Error` type, always typed.
- `async fn` methods where I/O is involved; no blocking-await on async-runtime threads.
- Documented retry posture per method (in rustdoc): does the method retry internally, expect the caller to retry, or fail fast?
- A reference implementation in the same crate that exercises every method and is used by `zally-testkit` for fixtures.

## Error vocabulary

`thiserror` v2 throughout. No `Box<dyn Error>`, no `anyhow`, no `Other(String)` catch-alls in any public Zally surface.

Each error variant has a documented retry posture in its rustdoc:

- **`retryable`** — same call with the same arguments may succeed on retry. Transient network failure, mempool eviction, RocksDB lock contention.
- **`not_retryable`** — same call will fail the same way. Invalid input, protocol violation, exhausted balance.
- **`requires_operator`** — caller cannot fix; an operator must intervene. Sealed-seed integrity failure, schema mismatch, lost wallet database.

The full enum is recorded in `docs/reference/error-vocabulary.md` (added as the first crate's errors land). A new error variant requires an entry there before merging.

Error names are `{Domain}Error`:

- `WalletError`, `ChainSourceError`, `SubmitterError`, `StorageError`, `SealingError`, `PcztError`.

Variants describe *what failed*, not *how*. `WalletError::MemoOnTransparentRecipient` is correct; `WalletError::InvalidArgument` is forbidden.

## Config and env var conventions

Zally is library-shaped today and does not own a TOML config. Crates that surface configuration (e.g., `ZinderChainSource` connection settings) do so through typed Rust builders, not through a config file Zally owns.

When operators integrating Zally do construct their own TOML or env-var layout, the recommended conventions match Zinder's:

- Env var prefix per consumer: a faucet might use `FAUZEC_`, a payment processor `PAY_`. Zally itself does not prescribe one.
- Test-only env vars (read by `zally-testkit`) use `ZALLY_TEST_*`. Production binaries strip these.
- Live-node gate: `ZALLY_TEST_LIVE=1`. Mainnet allowance: `ZALLY_TEST_ALLOW_MAINNET=1`.
- Sensitive leaves (`password`, `secret`, `token`, `key`, `seed`, `wif`) are never set via env var on their own; they come from a secret manager or sealed at-rest material.

## File and module conventions

Per [identifier-naming](https://github.com/gustavovalverde/zinder/blob/main/docs/architecture/public-interfaces.md) and the Zally codebase-structure rules:

- File names match the primary export. `wallet.rs` exports `Wallet`; `chain_source.rs` exports the `ChainSource` trait and its default implementation.
- No `mod.rs` files in new code. Use `{module_name}.rs` siblings.
- No `index.rs`, no `lib_internal.rs`, no other barrel-shaped names. Each file declares what it exports.
- No stuttering paths: inside the `keys/` directory the file is `sealing.rs`, not `keys_sealing.rs`.
- No single-file directories. If `keys/` contains only `sealing.rs`, promote to `keys_sealing.rs` at the crate root.

## Cross-domain disambiguation

When two domains use the same word, prefix with the bounded context or rename to what each thing actually computes. Two `pczt.rs` files at different crate roots would be ambiguous; one is `pczt-signer.rs`, the other is `pczt-roles.rs`.

The same word that means different things in Zally vs librustzcash gets disambiguated in Zally:

- librustzcash's `WalletRead`/`WalletWrite` → Zally's `WalletStorage` (single trait wrapping both).
- librustzcash's `BlockSource`/`BlockCache` → Zally's `ChainSource` (extended with tree state, transparent UTXO reads, transaction lookup, mempool peek; broader than librustzcash's minimal contract).
- librustzcash's `UnifiedSpendingKey` → Zally re-exports as-is (canonical name; not renamed).

## Stability

Zally's public API is the contract; librustzcash version churn behind Zally is not. Zally's own semver applies to:

- Every `pub` type, function, trait, enum variant, error variant, and trait method in every crate.
- The `Cargo.toml` `version` and `rust-version` fields.
- The validation gate (changes to the gate require an ADR amendment).

Internal modules (`pub(crate)`, private), test code, fixtures, and rustdoc examples are not covered by semver.

## When in doubt

The priority order: **honesty > specificity > vocabulary match > brevity**.

Prefer a longer name that is honest (`unseal_seed_with_age_identity`) over a shorter one that lies (`load_seed`). Prefer a specific verb from the table (`derive`) over a shorter generic one (`get`). Prefer match-existing-convention even when a competing convention is technically defensible — consistency beats local optimality.

If a name cannot be chosen without violating one of these rules, the abstraction is wrong, not the name. Reshape the abstraction.
