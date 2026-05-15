# Public Interfaces

This document defines Zally's public vocabulary: types, functions, error variants, trait methods, config fields, env vars, file names, and directories.

## Naming rules

### Forbidden roots, anywhere in any identifier

These words carry no bounded context. If a name needs them to be intelligible, the boundary is not understood.

- `utils`, `helpers`, `common`, `shared`, `manager`, `handler`, `processor`
- `data`, `info`, `item`, `result`, `stuff`, `thing`, `tmp`, `value`, `payload`
- `obj`, `foo`, `bar`

As suffixes on type names: `*Service`, `*Server`, `*Api`, `*Manager`, `*Processor`, `*Helper`, `*Util`, `*Data`, `*Info`. Kept: `*Error` (standard trait extension), `*Source` (read seam, e.g., `ChainSource`), `*Storage` (write seam, e.g., `WalletStorage`).

### Required suffixes on numeric and lifecycle identifiers

Numeric identifiers carry their unit in the name.

- Duration: `_ms`, `_seconds`, `_minutes`, `_hours`, `_blocks`, `_height`. Never bare `timeout`, `delay`, `interval`.
- Money: `_zat` for integer zatoshis, `_zec` for decimal-string ZEC. Never bare `amount`.
- Booleans: `is_*`, `has_*`, `can_*`. Affirmative only. Never `is_not_ready`, never `disabled` (use `is_disabled` only if the call sites read negatively).
- Counts: `_count` suffix on integer counts. Never bare `total`.
- Bytes: `_bytes` suffix when the unit is bytes. Never bare `size`.

### Network-tagged everywhere

The most catastrophic class of wallet bug is signing a testnet transaction with mainnet keys (or vice versa). Make the bug unrepresentable.

- Every public type that names an address, key, balance, or transaction carries a `Network` value or is generic over a `NetworkSpec` parameter.
- A function that takes an address also takes or carries a network.
- Constructors fail closed on mismatch. `Wallet::open(seed, Network::Testnet)` opens a testnet wallet; calling its methods with a mainnet address is a typed error at compile time where possible, at runtime otherwise.

### Verbs from a single vocabulary

Zally pins one verb per operation.

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
| `prove_*` | create zero-knowledge proofs for a PCZT without applying signatures |
| `sign_*` | apply signature material to a `Proposal` or `Pczt` |
| `submit_*` | broadcast a signed transaction to the network |
| `send_*` | high-level: propose + sign + submit in one call |
| `shield_*` | move wallet-owned transparent funds into a shielded receiver |
| `seal_*` / `unseal_*` | apply or remove at-rest encryption to seed material |
| `observe_*` | subscribe to events (`WalletEvent` stream) |
| `sync_*` | catch up wallet state with chain |
| `export_*` | serialise material for external consumption (UFVK, PCZT) |

Forbidden verbs for domain operations: `handle_*`, `process_*`, `manage_*`, `do_*`, `perform_*`, `execute_*`.

### No temporal or implementation drift

A name must survive a change of its implementation.

- Banned patterns: `new_x`, `x2`, `legacy_x`, `x_old`, `x_final`, `x_real`, `x_actual`, `x_improved`.
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
| `WalletStatus` | `zally-wallet` | Operator readiness snapshot derived from persisted wallet progress. |
| `SyncDriver` | `zally-wallet` | Caller-owned continuous sync loop over a `Wallet` and `ChainSource`. |
| `SyncSnapshot` | `zally-wallet` | Observable state emitted by a running `SyncDriver`. |
| `WalletStorage` | `zally-storage` | Trait abstraction over librustzcash's `WalletRead` + `WalletWrite`. |
| `ChainSource` | `zally-chain` | Trait for compact-block reads, tree state, transparent UTXO lookups. |
| `ChainEventCursor` | `zally-chain` | Opaque cursor for resuming chain-event streams without exposing backend internals. |
| `ChainEventEnvelope` | `zally-chain` | Cursor-bound chain event consumed by sync drivers. |
| `Submitter` | `zally-chain` | Trait for transaction broadcast. |
| `SeedSealing` | `zally-keys` | Trait for at-rest seed encryption. |
| `WalletCapabilities` | `zally-wallet` | Runtime descriptor of supported features (ZIP coverage, PCZT version, sealing impl). |

### Discriminated unions

For state-machine types, the discriminant field is `status:` (lifecycle) or `kind:` (variant):

```rust
pub enum SyncStatus {
    NotStarted,
    Starting { target_height: BlockHeight },
    CatchingUp { scanned_height: BlockHeight, target_height: BlockHeight, lag_blocks: u32 },
    AtTip { tip_height: BlockHeight },
    TipRegressed { scanned_height: BlockHeight, chain_tip_height: BlockHeight },
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
- A reference implementation in the same crate that exercises every method.

## Error vocabulary

`thiserror` v2 throughout. No `Box<dyn Error>`, no `anyhow`, no `Other(String)` catch-alls in any public Zally surface.

Each error variant has a documented retry posture in its rustdoc:

- **`retryable`**: same call with the same arguments may succeed on retry. Transient network failure, mempool eviction, RocksDB lock contention.
- **`not_retryable`**: same call will fail the same way. Invalid input, protocol violation, exhausted balance.
- **`requires_operator`**: caller cannot fix; an operator must intervene. Sealed-seed integrity failure, schema mismatch, lost wallet database.

The full enum is recorded in `docs/reference/error-vocabulary.md`. A new error variant requires an entry there before merging.

Error names are `{Domain}Error`:

- `WalletError`, `ChainSourceError`, `SubmitterError`, `StorageError`, `SealingError`, `PcztError`.

Variants describe what failed. `WalletError::MemoOnTransparentRecipient` is valid; `WalletError::InvalidArgument` is forbidden.

## Config and env var conventions

Zally is library-shaped and does not own a TOML config. Crates that surface configuration (e.g., `ZinderChainSource` connection settings) do so through typed Rust builders, not through a config file Zally owns.

When operators integrating Zally construct their own TOML or env-var layout:

- Env var prefix per consumer: for example, `WALLET_` or an application-specific product prefix. Zally itself does not prescribe one.
- Test-only env vars (read by `zally-testkit`) use `ZALLY_TEST_*`. Production binaries strip these.
- Live-node gate: `ZALLY_TEST_LIVE=1`. Mainnet allowance: `ZALLY_TEST_ALLOW_MAINNET=1`.
- Sensitive leaves (`password`, `secret`, `token`, `key`, `seed`, `wif`) are never set via env var on their own; they come from a secret manager or sealed at-rest material.

## File and module conventions

Per the Zally identifier-naming and codebase-structure rules:

- File names match the primary export. `wallet.rs` exports `Wallet`; `chain_source.rs` exports the `ChainSource` trait and its default implementation.
- No `mod.rs` files. Use `{module_name}.rs` siblings.
- No `index.rs`, no `lib_internal.rs`, no other barrel-shaped names. Each file declares what it exports.
- No stuttering paths: inside the `keys/` directory the file is `sealing.rs`, not `keys_sealing.rs`.
- No single-file directories. If `keys/` contains only `sealing.rs`, promote to `keys_sealing.rs` at the crate root.

## Cross-domain disambiguation

When two domains use the same word, prefix with the bounded context or rename to what each thing actually computes. Two `pczt.rs` files at different crate roots would be ambiguous; one is `pczt-signer.rs`, the other is `pczt-roles.rs`.

The same word that means different things in Zally vs librustzcash gets disambiguated in Zally:

- librustzcash's `WalletRead`/`WalletWrite` becomes Zally's `WalletStorage` (single trait wrapping both).
- librustzcash's `BlockSource`/`BlockCache` becomes Zally's `ChainSource` (extended with tree state, transparent UTXO reads, transaction lookup, mempool peek; broader than librustzcash's minimal contract).
- librustzcash's `UnifiedSpendingKey` is re-exported as-is (canonical name; not renamed).

## Stability

Zally's public API is the contract; librustzcash version churn behind Zally is not. Zally's own semver applies to:

- Every `pub` type, function, trait, enum variant, error variant, and trait method in every crate.
- The `Cargo.toml` `version` and `rust-version` fields.
- The validation gate (changes to the gate require an ADR amendment).

Internal modules (`pub(crate)`, private), test code, fixtures, and rustdoc examples are not covered by semver.

## Capability surface

The contract surface Zally publishes, grouped by domain. Each item is a guarantee the public API enforces.

### Wallet core (CORE)

- **CORE-1**: Generate a fresh wallet (mnemonic plus ZIP-32 derivation) for any network. Single async call.
- **CORE-2**: Open an existing wallet from a sealed seed; fail closed on integrity error.
- **CORE-3**: One operator identity per process; the wallet has one account per `Wallet::create`.
- **CORE-4**: UA derivation per ZIP-316; UFVK and UIVK export.
- **CORE-5**: Idempotent send by `IdempotencyKey`.
- **CORE-6**: Network-tagged types throughout; compile error if a network is missing.

### Key management (KEYS)

- **KEYS-1**: `SeedSealing` trait with `AgeFileSealing` as default.
- **KEYS-2**: `unsafe_plaintext_seed` opt-in with `WARN`-level log on every open.
- **KEYS-3**: USK derived from seed at signing time; zeroized after use.
- **KEYS-4**: UFVK / UIVK export with stable serialization; spending key never serialized through Zally's public API.

### Chain source and submission (CHAIN)

- **CHAIN-1**: `ChainSource` trait with methods for compact-block range fetch, latest tree state, transaction lookup, address-UTXO read, and cursor-bound chain events.
- **CHAIN-2**: `Submitter` trait with a single `submit(raw_tx) -> SubmitOutcome` method.
- **CHAIN-3**: Reconnect, retry, and circuit-breaker logic at the wallet boundary, not duplicated in each embedding application.
- **CHAIN-4**: `ChainEventCursor` is opaque Zally vocabulary. Backend cursor types never cross the public Zally API.

### Sync and scanning (SYNC)

- **SYNC-1**: Catch up to chain tip on `Wallet::sync(...)`.
- **SYNC-2**: Incremental sync emits `tracing` events with scan progress and per-pool note counts.
- **SYNC-3**: Reorg handling: on continuity error, automatic rollback to the longest common prefix; emit `WalletEvent::ReorgDetected`.
- **SYNC-4**: Configurable confirmation depth per receiver purpose (default 1 for non-coinbase, mandatory 100 for coinbase per ZIP-213).
- **SYNC-5**: `WalletEvent` async stream for `ShieldedReceiveObserved`, `TransactionConfirmed`, `ReorgDetected`, `ScanProgress`.
- **SYNC-6**: `SyncDriver` provides a caller-owned continuous sync loop over `Wallet::sync`, `ChainSource::chain_event_envelopes`, polling fallback, and bounded per-sync timeouts.
- **SYNC-7**: `Wallet::status_snapshot() -> WalletStatus` reports `SyncStatus`, scan height, observed tip, lag, subscriber count, and circuit-breaker state.
- **SYNC-8**: `Wallet::open_or_create_account(...) -> (Wallet, AccountId)` opens the sealed-seed account or creates it on a fresh storage volume.
- **SYNC-9**: `Wallet::sync(...)` refreshes wallet-owned transparent UTXOs through `ChainSource::transparent_utxos` because compact blocks do not expose transparent receive details.

### Spending (SPEND)

- **SPEND-1**: `Wallet::send_payment(SendPaymentPlan) -> SendOutcome`. Transparent and shielded recipients both supported.
- **SPEND-2**: Refuse memo on transparent recipient (ZIP-302) at API call; typed error variant.
- **SPEND-3**: TEX address support per ZIP-320: model TEX recipients distinctly and reject memos as transparent recipients.
- **SPEND-4**: Conventional fee per ZIP-317.
- **SPEND-5**: `nExpiryHeight` set per ZIP-203.
- **SPEND-6**: ZIP-321 payment URI parsing through `PaymentRequest::from_uri`.
- **SPEND-7**: `Wallet::shield_transparent_funds(ShieldTransparentPlan) -> SendOutcome` explicitly shields wallet-owned transparent UTXOs before shielded spending.

### PCZT and external signing (PCZT)

- **PCZT-1**: `Wallet::propose_pczt(plan) -> PcztBytes`: build an unsigned PCZT without holding spending keys.
- **PCZT-2**: PCZT serialization to bytes for export to HSM, FROST coordinator, air-gapped signer.
- **PCZT-3**: `Wallet::prove_pczt(pczt) -> PcztBytes`: create required Sapling and Orchard proofs for the in-process path.
- **PCZT-4**: `Wallet::sign_pczt(pczt) -> PcztBytes`: sign in-process (uses USK).
- **PCZT-5**: `Wallet::extract_and_submit_pczt(final, submitter) -> SendOutcome`: extract the transaction from a fully-authorized PCZT and submit.
- **PCZT-6**: PCZT support covers transparent, Sapling, and Orchard bundles.

### Observability (OBS)

- **OBS-1**: explicit `tracing` events at meaningful state transitions with documented event names and fields. No blanket method-entry instrumentation.
- **OBS-2**: `Wallet::metrics_snapshot() -> WalletMetrics` returns a typed metrics snapshot derived from the same persisted progress as `WalletStatus`.
- **OBS-3**: `WalletEvent` async stream documented as the canonical push notification channel for state changes.
- **OBS-4**: `SyncHandle::observe_status() -> SyncSnapshotStream` is the canonical push channel for sync-driver lifecycle state.

### Documentation and DX (DOC)

- **DOC-1**: Every public item has rustdoc. CI enforces with `RUSTDOCFLAGS='-D warnings'`.
- **DOC-2**: `examples/` directory contains cookbook entries: `open-wallet`, `exchange-deposit`, `payment-processor`, `custody-with-pczt`, `mining-payout`, `live-zinder-probe`. Each example compiles and runs.

### Integration experience (IX)

- **IX-1**: `WalletCapabilities` descriptor exposing typed feature flags; new capabilities are additive enum variants under `#[non_exhaustive]`.
- **IX-2**: Every error variant has a documented retry posture (`retryable`, `not_retryable`, `requires_operator`).
- **IX-3**: Examples are self-contained; each `examples/<name>/main.rs` runs end-to-end without external prerequisite shell scripts.

## ZIP compliance surface

Which ZIPs Zally implements directly, which it inherits through librustzcash, and which are out of scope.

### Implemented

- **ZIP-32**: Shielded HD wallets. Multi-account derivation under standard paths.
- **ZIP-200**: Network upgrade mechanism. Active branch ID consulted at transaction construction time.
- **ZIP-203**: Transaction expiry. `nExpiryHeight` set on every transaction.
- **ZIP-212**: `rseed` note plaintext encoding. Inherited from librustzcash.
- **ZIP-213**: Shielded coinbase plus 100-block maturity rule. Enforced at spending input selection.
- **ZIP-225**: v5 transaction format. The default on every supported network.
- **ZIP-244**: Non-malleable TxID. Tracked in wallet state; exposed via `WalletEvent` and `SendOutcome`.
- **ZIP-302**: Memo encoding (current 512-byte format). Encoded and decoded per spec.
- **ZIP-315**: Wallet best practices. Explicit transparent shielding, trusted vs untrusted TXO accounting, confirmation-depth defaults aligned with the ZIP.
- **ZIP-316**: Unified Addresses, UFVKs, UIVKs. First-class address type; UFVK export.
- **ZIP-317**: Conventional fee mechanism.
- **ZIP-320**: TEX addresses. Recognised as transparent recipients at the API boundary.
- **ZIP-321**: Payment request URIs. First-class `PaymentRequest` type.
- **ZIP-401**: Mempool DoS protection. Submission retries respect mempool eviction.

### Reserved By Shape

- **ZIP-230**: v6 transaction format (NU7, OrchardZSA). Transaction construction stays behind wallet/storage methods so a v6 path does not leak librustzcash internals.
- **ZIP-231**: Memo bundles. Memo API takes an opaque `Memo` value rather than exposing a Zally-owned memo enum.
- **ZIP-312**: FROST spend authorisation. PCZT signing stays in `zally-pczt`, separate from wallet sync and storage.

### Out of scope

- **ZIP-48**: Transparent multisig wallets.
- **ZIP-211**: Sprout pool sunset. Sprout is dead; Zally never deposits to Sprout. Sprout sweeps are out of scope.
- **ZIP-308**: Sprout-to-Sapling migration (see ZIP-211).
- **ZIP-311**: Payment disclosures.

## Naming Priority

Priority order: specificity, vocabulary match, then brevity. Use the domain verb table and unit suffix rules before introducing a new naming pattern.
