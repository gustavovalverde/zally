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
| `get_*` | reads a single authoritative record by key; returns the record or a typed error. Storage I/O is allowed; chain calls are not. |
| `list_*` | returns an ordered collection of existing records without advancing any state or performing a new chain call. |
| `find_*` | lookup that may return `None`. Both single-key and batch lookups (sparse maps) are `find_*`. |
| `compute_*` | pure deterministic calculation, no I/O. |
| `derive_*` | protocol-level derivation (cryptographic, ZIP-32 keys, ZIP-244 digests). |
| `resolve_*` | polymorphic dispatch returning one authoritative value. |
| `open_*` / `close_*` | wallet or driver lifecycle. |
| `create_*` | allocate or insert a new entity. |
| `record_*` | persist a fact derived from an external event (broadcast emitted, tip observed, receive seen). Idempotent on the natural key. |
| `clear_*` | remove rows from a write-through filter or cache table (does not delete user data). |
| `truncate_*` | roll persisted state back to a height, discarding rows above. |
| `update_*` | replace a tracked value in place (chain tip, retry policy). |
| `propose_*` | build a `Proposal` without signing (librustzcash vocabulary). |
| `prepare_*` | build, sign, and persist a transaction without broadcasting (intermediate step in the spend pipeline). |
| `prove_*` | create zero-knowledge proofs for a PCZT without applying signatures. |
| `sign_*` | apply signature material to a `Proposal` or `Pczt`. |
| `submit_*` | broadcast a signed transaction to the network. |
| `send_*` | high-level: propose + sign + submit in one call. |
| `shield_*` | move wallet-owned transparent funds into a shielded receiver. |
| `abandon_*` | terminate an incomplete artifact lifecycle and release its exact reservations. |
| `seal_*` / `unseal_*` | apply or remove at-rest encryption to seed material. |
| `observe_*` | subscribe to events (`WalletEvent` stream). |
| `sync_*` | catch up wallet state with chain. |
| `scan_*` | run the block scanner against a batch of compact blocks. |
| `export_*` | serialise material for external consumption (UFVK, PCZT). |
| `extract_*` | parse a serialised artifact (PCZT) back into a typed value. |

Forbidden verbs for domain operations: `handle_*`, `process_*`, `manage_*`, `do_*`, `perform_*`, `execute_*`, `lookup_*` (use `find_*`), `fetch_*` (use `get_*` or `find_*`).

### No temporal or implementation drift

A name must survive a change of its implementation.

- Banned patterns: `new_x`, `x2`, `legacy_x`, `x_old`, `x_final`, `x_real`, `x_actual`, `x_improved`.
- Banned implementation leaks at the trait surface: a trait named after a backend (`SqliteStorage`, `ZinderChain`) ties the abstraction to one implementation. The trait is `WalletStorage` with `Sqlite` as one impl in `zally-storage`; the trait is `ChainSource` with `ZinderChainSource` as one impl in `zally-chain`. Implementation types are free to name the backend (`Sqlite`, `Zinder`) since they describe a single concrete backend, but the trait never does.
- Protocol names are domain, not implementation, and stay: `pczt`, `frost`, `zip32`, `zip316`, `zip321`, `zip317`, `zip320`, `tex`. These appear verbatim in identifiers.

## Type conventions

### Domain types

| Type | Crate | Role |
|---|---|---|
| `Network` | `zally-core` | Enum: `Mainnet`, `Testnet`, `Regtest`. Every public type carrying chain state is tagged. |
| `Zatoshis` | `zally-core` | `u64` non-negative newtype for transaction amounts. Never bare `u64`. |
| `BlockHeight` | `zally-core` | `u32` newtype. Never bare `u32` for height. |
| `BranchId` | `zally-core` | ZIP-200 branch identifier; required at transaction construction time. |
| `TxId` | `zally-core` | Non-malleable txid per ZIP-244. Never bare `[u8; 32]`. Text boundaries (`Display`, `Debug`, `FromStr`, `to_rpc_hex`, `from_rpc_hex`) use RPC byte order; `from_bytes` / `as_bytes` use internal byte order. See [ADR-0004](../adrs/0004-rpc-byte-order-hex-conversion.md). |
| `BlockHash` | `zally-core` | Canonical Zcash block hash. Never bare `[u8; 32]`. Same byte-order discipline as `TxId`. See [ADR-0004](../adrs/0004-rpc-byte-order-hex-conversion.md). |
| `FromRpcHexError` | `zally-core` | Shared error returned when parsing `TxId` or `BlockHash` from RPC byte order hex. |
| `AccountId` | `zally-core` | Opaque identifier for an account within a wallet. |
| `ReceiverPurpose` | `zally-core` | Enum naming each receiver's role (`Mining`, `Donations`, `HotDispense`, `ColdReserve`, `Custom(String)`). |
| `IdempotencyKey` | `zally-core` | Caller-supplied identifier for send-idempotency. `AsRef<str>` newtype. |
| `HoldId` | `zally-core` | Wallet-issued opaque identifier for a `Hold`. UUID v4 by construction; safe to log. |
| `Memo` | `zally-core` | ZIP-302 memo wrapper. Refuses construction over 512 bytes. |
| `TransparentGapLimit` | `zally-core` | BIP-44 gap-limit policy (`external`, `internal`, `ephemeral`). Zally raises the external/ephemeral defaults above the upstream `GapLimits::default()` to avoid Sapling-diversifier-driven first-reservation failures on a randomly-seeded fresh wallet. |
| `Wallet` | `zally-wallet` | The operator-facing handle. Async API. |
| `AccountBalance` | `zally-wallet` | Per-pool balance snapshot anchored to the persisted visible tip; network-tagged with mature/immature transparent split. |
| `ExposedAddress` | `zally-wallet` | Previously-derived Unified Address in derivation order, with diversifier index and transparent-receiver flag. |
| `PendingTransparentInputs` | `zally-wallet` | Snapshot of transparent outpoints locked by wallet-owned broadcasts not yet observed mined. |
| `PendingTransparentInput` | `zally-wallet` | Single outpoint spent by an unsettled broadcast, with `broadcast_tx_id`, `broadcast_at_ms`, `broadcast_at_height`, and `is_stale`. The same field names appear on the storage-side row; no rename across layers. |
| `Hold` | `zally-wallet` | Amount lock returned by `Wallet::reserve_for_dispense`. Carries the wallet-issued `HoldId`, the caller-supplied `request_id` and `idempotency_key`, `amount_zat`, `locked_notes_summary`, and `available_after_release_zat`. |
| `LockedNotesSummary` | `zally-wallet` | Count and total value of the shielded notes the wallet considered locked at reservation time. Informational; reservations enforce by amount, not by note identity. |
| `ReserveForDispensePlan` | `zally-wallet` | Inputs to `Wallet::reserve_for_dispense`: `account_id`, `amount_zat`, `request_id`, `idempotency_key`. |
| `SignedPayload` | `zally-core` | Chain-neutral envelope returned by wallet-runtime signers and consumed by facilitators. `bytes` carry the opaque signed artifact, `format` names how to extract or broadcast it, and chain-specific amounts stay in the `Amount` decimal-string shape. |
| `SignedPayloadFormat` | `zally-core` | Wire discriminant for `SignedPayload::bytes`. Zcash x402 exact uses `pczt-v2-extractable`; older PCZT v1 integrations use `pczt-v1`. |
| `WalletOptions` | `zally-wallet` | Open-time wallet configuration; currently carries the age after which pending broadcast inputs are flagged stale. |
| `ExportPaymentDisclosurePlan` | `zally-wallet` | Network-tagged recipient, transaction, amount, message, and explicit ZIP-311 profile used by `Wallet::export_payment_disclosure`. |
| `PaymentDisclosure` | `zally-wallet` | Network-tagged wrapper around the portable, canonically encoded disclosure. Its `Debug` output omits messages, proofs, and outgoing cipher keys. |
| `OutPoint` | `zally-core` | Transparent transaction outpoint (`tx_id` + `output_index`). Zally-owned so the public surface composes only Zally domain types; inside `zally-storage` the upstream `zcash_transparent::bundle::OutPoint` is imported as `UpstreamOutPoint` to avoid the same-name collision. |
| `WalletStatus` | `zally-wallet` | Operator readiness snapshot derived from persisted wallet progress. |
| `SyncDriver` | `zally-wallet` | Caller-owned continuous sync loop over a `Wallet` and `ChainSource`. |
| `SyncSnapshot` | `zally-wallet` | Observable state emitted by a running `SyncDriver`. |
| `SyncObservation` | `zally-wallet` | Chain state the wallet has committed and checked against the chain, and when. Carried on `SyncSnapshot::last_observation`; the age input to a consumer's spend-freshness gate. |
| `WalletStorage` | `zally-storage` | Trait abstraction over librustzcash's `WalletRead` + `WalletWrite`. |
| `PendingBroadcastTransaction` | `zally-storage` | A wallet-owned broadcast whose fate the chain has not settled: transaction ID, stored bytes, and mined height. Returned by `WalletStorage::list_pending_broadcast_transactions`. |
| `ProposalRejectionKind` | `zally-storage`, re-exported by `zally-wallet` | Why a proposal or its transaction could not be built, with its posture. Carried by `WalletError::ProposalRejected` and `StorageError::ProposalRejected`; see the [error vocabulary](../reference/error-vocabulary.md#proposalrejectionkind). |
| `ShieldedBlockLimit` | `zally-storage`, re-exported by `zally-wallet` | The ZIP 218 per-block limit a proposal exceeded: Orchard actions, Ironwood actions, Sapling spends and outputs, or all shielded pools together. Carried by `ProposalRejectionKind::ShieldedLimitExceeded`. |
| `ChainSource` | `zally-chain` | Trait for compact-block reads, tree state, transparent UTXO lookups. |
| `ChainEventCursor` | `zally-chain` | Opaque cursor for resuming chain-event streams without exposing backend internals. |
| `ChainEventStreamStart` | `zally-chain` | Explicit chain-event subscription start position (`AfterCursor` \| `EarliestRetained` \| `LiveTail`). |
| `ChainEventEnvelope` | `zally-chain` | Cursor-bound chain event consumed by sync drivers. |
| `Submitter` | `zally-chain` | Trait for transaction broadcast. |
| `TransactionParseError` | `zally-chain` | Typed rejection of malformed serialized transaction bytes at the chain-plane boundary. |
| `TransactionHeader` | `zally-chain` | Version, consensus branch, and expiry height of a serialized transaction or a PCZT. `check_mineable` says whether any block after a given tip can still include it. |
| `UnmineableReason` | `zally-chain` | Why a transaction can never be mined: `Expired`, `ConsensusBranchMismatch`, or `VersionNotAccepted`. Carried by `WalletError::TransactionUnmineable`. |
| `SeedSealing` | `zally-keys` | Trait for at-rest seed encryption. |
| `WalletCapabilities` | `zally-wallet` | Runtime descriptor of supported features (ZIP coverage, PCZT version, sealing impl). |

### Byte order for hash newtypes

A 32-byte Zcash hash (txid, block hash) exists in two byte orders. Zally exposes both in one place so consumers never reinvent the conversion:

- **Internal byte order**: the raw SHA-256d output bytes used in consensus serialization. Reference: Zcash protocol spec, protocol.tex:13560-13564. This is what `TxId::from_bytes` / `as_bytes` and `BlockHash::from_bytes` / `as_bytes` carry; storage and direct consensus-serialization paths use these accessors.
- **RPC byte order**: the byte-reversed form every `zcash-cli` reply, every wallet UI, every block explorer URL, and the specification itself print. Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, defining sentence at :4036).

The rule: Zally's hash newtypes accept and emit RPC byte order at every text boundary (`Display`, `Debug`, `FromStr`, `to_rpc_hex`, `from_rpc_hex`); internal byte order is reserved for the `from_bytes` / `as_bytes` accessors that exist for storage and direct consensus serialization. See [ADR-0004](../adrs/0004-rpc-byte-order-hex-conversion.md) for rationale.

Every new `[u8; 32]` hash newtype in `zally-core` ships with the same surface; new hash kinds reuse [`FromRpcHexError`](../../crates/zally-core/src/hash_hex.rs).

### Discriminated unions

For state-machine types, the discriminant field is `status:` (lifecycle) or `kind:` (variant):

```rust
pub enum SyncStatus {
    NotStarted,
    Starting { target_height: BlockHeight },
    CatchingUp { scanned_height: BlockHeight, target_height: BlockHeight, lag_blocks: u32 },
    AtTip { visible_tip_height: BlockHeight },
    TipRegressed { scanned_height: BlockHeight, visible_tip_height: BlockHeight },
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
- Explicit Sapling proving-parameter paths are selected through `SqliteOptions::with_sapling_proving_parameters`; omitting them uses the platform-default parameter directory.
- Test-only env vars (read by `zally-testkit`) use `ZALLY_TEST_*`. Production binaries strip these.
- Live-node gate: `ZALLY_TEST_LIVE=1`. Mainnet allowance: `ZALLY_TEST_ALLOW_MAINNET=1`.
- Sensitive leaves (`password`, `secret`, `token`, `key`, `seed`, `wif`) are never set via env var on their own; they come from a secret manager or sealed at-rest material.

## File and module conventions

Per the Zally identifier-naming and codebase-structure rules:

- File names match the primary export, with the crate-context prefix dropped. `zally-wallet/src/wallet.rs` exports `Wallet`; `zally-chain/src/source.rs` exports `ChainSource`; `zally-chain/src/error.rs` exports `ChainSourceError` and `SubmitterError`; `zally-wallet/src/error.rs` exports `WalletError`. The type retains the bounded-context prefix (`WalletError`, `ChainSourceError`, `PcztError`) so cross-workspace grep stays exact; the file drops it so in-crate paths stay short.
- No `mod.rs` files. Use `{module_name}.rs` siblings.
- No `index.rs`, no `lib_internal.rs`, no other barrel-shaped names. Each file declares what it exports.
- No stuttering paths: inside `zally-pczt/src/` the error file is `error.rs`, not `pczt_error.rs`; inside `zally-chain/src/` the Zinder adapter is `zinder_source.rs`, not `zinder_chain_source.rs`.
- No single-file directories. If `keys/` contains only `sealing.rs`, promote to `keys_sealing.rs` at the crate root.

## Cross-domain disambiguation

When two domains use the same word, prefix with the bounded context or rename to what each thing actually computes. Two `pczt.rs` files at different crate roots would be ambiguous; one is `pczt_signer.rs`, the other is `pczt_roles.rs`.

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

`WalletStorage` is a public trait, so a new required method breaks every implementation outside Zally. The NU7 activation guards made these breaking changes:

- Added `list_pending_broadcast_transactions`, which returns `PendingBroadcastTransaction` values, and `record_unmineable_transaction`.
- Removed `clear_expired_pending_broadcast_inputs`, because wall-clock age no longer retires a pending broadcast.
- `find_idempotent_submission` returns `None`, and `record_idempotent_submission` rebinds the key, once the key's transaction is recorded as unmineable.

Fork-point rewinds and the ledger-preserving rebuild made these breaking changes:

- Replaced `truncate_to_chain_state` with `revert_blocks_above`, which also records the reverted broadcasts and the applied chain-event cursor and returns `RevertedBlocks`.
- Added `find_highest_scanned_height`, `find_scanned_block_hash`, `find_chain_event_cursor`, `record_chain_event_cursor`, `clear_chain_event_cursor`, `list_reverted_broadcasts`, `record_reverted_broadcast_resubmitted`, and `clear_reverted_broadcast`.
- `record_unmineable_transaction` also drops the transaction's reverted-broadcast row and settles the holds finalized with it, returning them as `UnmineableHold` values. `list_pending_broadcast_transactions` also returns reverted broadcasts.
- `recreate_with_account` keeps the payment ledger.

The minimum source reorg window made this breaking change to `ChainSource`, a public trait:

- Added `reorg_window_blocks`, the depth of the deepest reorg the source follows.

The minimum itself is `WalletOptions::minimum_source_reorg_window_blocks`. SYNC-10 also requires `ChainSource::transaction_status` to report a transaction not found before it records it unmineable, so a source that cannot answer that lookup never frees a key.

Binding the idempotency key before submission made these breaking changes:

- Added `record_pending_submission`, which binds a key to the transaction that pays and records every transaction of the send as its steps, with their pending-broadcast inputs, in one storage transaction, and `release_pending_submission`, which undoes it for transactions that can never be mined.
- Added `list_send_steps`, `record_confirmed_broadcast`, and `store_carried_send_steps`, which return a bound send's steps, record a broadcast the chain source reports mined, and store a rebuilt wallet's carried steps before it rescans.

## Capability surface

The contract surface Zally publishes, grouped by domain. Each item is a guarantee the public API enforces.

### Wallet core (CORE)

- **CORE-1**: Generate a fresh wallet (mnemonic plus ZIP-32 derivation) for any network. Single async call.
- **CORE-2**: Open an existing wallet from a sealed seed; fail closed on integrity error.
- **CORE-3**: One operator identity per process; the wallet has one account per `Wallet::create`.
- **CORE-4**: UA derivation per ZIP-316; UFVK and UIVK export. Previously exposed Unified Addresses are queryable in derivation order via `Wallet::list_exposed_addresses` without advancing the diversifier index or consuming BIP-44 transparent gap-limit slots.
- **CORE-5**: Idempotent send by `IdempotencyKey`. `Wallet::send_payment` and `Wallet::shield_transparent_funds` return the transaction a key already names while it is pending or mined, without building another. The key is bound to the transaction that pays, the last step of a multi-step (ZIP 320) proposal, before anything is submitted, in the storage transaction that records the send's pending-broadcast inputs. A lost submit acknowledgement, a crash after the broadcast, or a node rejection therefore leaves the key bound, and a retry returns that transaction. A retry first submits the bound transaction again while the wallet has not seen it mined, after every earlier step of its send the wallet has not seen mined, in submission order, so `Ok` means a node accepted, holds, or mined it; a submit error or rejection is returned and the key stays bound. A bound transaction the next block can no longer include fails with `WalletError::PendingBroadcastUnsettled` (retryable) until SYNC-10 settles it. A key that already names a transaction answers with it even while new spends are refused (SYNC-13, SYNC-14). The binding is released at once only when the paying transaction can no longer be mined: the send stopped before it reached a submitter, or a node rejected a step for a reason that holds for every block. No rejection reason the chain source reports qualifies yet. Once SYNC-10 records the transaction as unmineable, the key builds one replacement on its next send; that verdict is final only while the chain source's reorg window covers every reorg the node can produce (SYNC-14). Sends that share a key through one wallet handle and its clones run one at a time, so concurrent retries return the same transaction; the database admits one writer (CORE-7).
- **CORE-6**: Network-tagged types throughout; compile error if a network is missing.
- **CORE-7**: One writer per wallet database. A `Sqlite` storage handle takes an exclusive advisory lock on the sibling file `<db_path>.lock` when it opens the database and holds it until its last clone is dropped. Another handle on the same file, in this process or another, fails to open with `StorageError::DatabaseLocked`, after waiting up to two seconds for a handle that is closing. Run one `Wallet` per database and clone it for concurrent use: clones share the per-key send lock (CORE-5), and a rebuild (ADR-0005 decision 10) replaces the file under no other writer. Two `Wallet` values built over clones of one storage handle are not refused and still cannot pay twice, because the second binding of a key conflicts, but a concurrent retry through the other value then fails with `IdempotencyKeyConflict` instead of returning the bound transaction.

Unified Address strings are encoded at ZIP-316 Revision 0 when Revision 0 can represent the address, and at Revision 2 otherwise. Addresses that Zally derives always carry a shielded receiver and no metadata, so they encode at Revision 0 (for example, Regtest `uregtest1…`). A recipient may still present a Revision 2 address, whose prefixes differ (for example, `zuregtest1…` or `turegtest1…`). Callers should parse the address and validate its network instead of classifying it by a fixed prefix. `AccountId` values derive from the Revision 0 UFVK encoding and stay stable across upstream encoder changes.

### Key management (KEYS)

- **KEYS-1**: `SeedSealing` trait with `AgeFileSealing` as default.
- **KEYS-2**: `unsafe_plaintext_seed` opt-in with `WARN`-level log on every open.
- **KEYS-3**: USK derived from seed at signing time; zeroized after use.
- **KEYS-4**: UFVK / UIVK export with stable serialization; spending key never serialized through Zally's public API.

### Chain source and submission (CHAIN)

- **CHAIN-1**: `ChainSource` trait with source-neutral `CompactBlockArtifact` and `TreeStateArtifact` values, epoch-pinned range and tree-state reads, transaction lookup, address-UTXO reads, and cursor-bound chain events. Librustzcash and lightwalletd protocol types remain private to storage conversion and compatibility services.
- **CHAIN-2**: `Submitter` trait with a single `submit(raw_tx) -> SubmitOutcome` method.
- **CHAIN-3**: Reconnect, retry, and circuit-breaker logic at the wallet boundary, not duplicated in each embedding application.
- **CHAIN-4**: `ChainEventCursor` is opaque Zally vocabulary. Backend cursor types never cross the public Zally API.
- **CHAIN-5**: A chain-event subscription states its start position explicitly through `ChainEventStreamStart` (`AfterCursor` | `EarliestRetained` | `LiveTail`); there is no implicit empty-cursor default.
- **CHAIN-6**: `parse_transaction_expiry_height` parses a serialized transaction through the same librustzcash family as the chain plane and returns its `BlockHeight` without pulling a storage backend into the caller.
- **CHAIN-7**: `parse_transaction_header` returns a transaction's `TransactionHeader`, and `TransactionHeader::from_pczt_global` builds one from a PCZT's global fields. `TransactionHeader::check_mineable(network, tip_height)` judges the block after the tip with `BranchId::for_height` and `TxVersion::valid_in_branch`: it rejects a transaction that expired, that commits to a consensus branch the chain has left, or whose version the branch no longer accepts. A transaction committed to a branch that activates later is not rejected.

### Sync and scanning (SYNC)

- **SYNC-1**: Catch up to chain tip on `Wallet::sync(...)`, which records the chain tip and scans one bounded chunk. `Wallet::scan_queued_range(...)` scans one bounded chunk of already-queued work; a caller draining a backlog calls `sync` once, then `scan_queued_range` until a run reports no blocks, then `sync` again. Only a `sync` reporting no blocks means the wallet reached the chain.
- **SYNC-2**: Incremental sync emits `tracing` events with scan progress and per-pool note counts.
- **SYNC-3**: Reorg handling: `SyncDriver` rewinds an announced `ChainReorged` to its fork point (`reverted.start_height - 1`) at any depth before the next scan, unless the wallet already follows the replacement chain. A continuity error no event announced walks the relative rewind ladder (10, 100, 1,000 blocks) before a rebuild from the birthday. Every rewind emits `WalletEvent::ReorgDetected`, `TransactionReverted`, and `ShieldedReceiveReverted`, and keeps the payment ledger. See [ADR-0005](../adrs/0005-wallet-sync-resume-anchor-cadence.md) decisions 4, 9, and 10.
- **SYNC-4**: Configurable confirmation depth per receiver purpose (default 1 for non-coinbase, mandatory 100 for coinbase per ZIP-213).
- **SYNC-5**: `WalletEvent` async stream for `ScanProgress`, `ShieldedReceiveObserved`, `ShieldedReceiveReverted`, `TransactionConfirmed`, `TransactionReverted`, `ReorgDetected`, `DerivedStateReset`, `TransparentUtxoRefreshFaulted`, `PendingBroadcastUnmineable`, `HoldDispenseUnmineable`, `SourceHalted`, `SourceRecovered`, `SourceReorgWindowTooShallow`, and `SourceReorgWindowSufficient`. The stream injects `Lagged` when a consumer falls behind.
- **SYNC-6**: `SyncDriver` performs one immediate catch-up, then starts wallet sync only for delivered chain events or explicit expired-cursor reconciliation. Timer ticks are limited to bounded stream reconnect, backoff, and scan-park reprobe work. Stream-open failures never park the driver; the first successful reopen after a failure streak triggers one catch-up sync.
- **SYNC-7**: `Wallet::status_snapshot() -> WalletStatus` reports `SyncStatus`, scan height, observed tip, lag, subscriber count, and circuit-breaker state.
- **SYNC-8**: `Wallet::open_or_create_account(...) -> (Wallet, AccountId)` opens the sealed-seed account or creates it on a fresh storage volume.
- **SYNC-9**: `Wallet::sync(...)` refreshes wallet-owned transparent UTXOs through `ChainSource::transparent_utxos` because compact blocks do not expose transparent receive details.
- **SYNC-10**: Each `Wallet::sync` and `Wallet::scan_queued_range` settles pending broadcasts at the judged height, the lower of the fully scanned height and the settled tip. The pending broadcasts are the transactions with pending-broadcast rows and the unmined transactions an idempotency key still names. A broadcast mined at or below the judged height loses its pending-broadcast rows. A broadcast that CHAIN-7 rules out for every block above it, such as one signed for NU6.3 while NU7 activated, and that `ChainSource::transaction_status` reports not found, is recorded as unmineable: in one storage transaction its pending-broadcast rows are dropped and its idempotency keys become free for a replacement (CORE-5). `WalletEvent::PendingBroadcastUnmineable` reports it exactly once, and the host recreates the payment. Nothing else retires a pending broadcast; wall-clock age never does, because the settled tip can trail the chain by the source's whole reorg window. Judging at the judged height means a broadcast that an unscanned block, or a reorg within the chain source's bound, could still mine is never reported. The verdict is final only while that bound covers every reorg the node can produce, which SYNC-14 enforces. librustzcash keeps excluding an unmineable transaction's inputs from selection until its expiry height passes. The source lookup guards against a mined transaction the wallet's scan missed: one the source reports mined is recorded mined at that height, with the earlier steps of its send, and keeps its key, and one the source still holds, or a failed lookup, stays unjudged until a later sync. A chain source that cannot answer the lookup therefore never frees a key. After a birthday rebuild, the wallet stores the carried steps of every unsettled multi-step send before it scans, so the rescan recognizes a paying step mined in the same block as the step that funds it.
- **SYNC-11**: `SyncDriver` persists the cursor of every chain event it has applied, atomically with the rewind for a reorg, and resumes with `ChainEventStreamStart::AfterCursor`. An expired (`ChainEventCursorExpired`) or unrecognized (`ChainEventCursorInvalid`) cursor is discarded in favor of `EarliestRetained`.
- **SYNC-12**: A transaction a rewind removed from the chain is a pending broadcast again and is settled only by the SYNC-10 verdict, including after a rebuild. `SyncDriver::with_submitter` resubmits a reverted withdrawal once while the next block can include it.
- **SYNC-13**: A chain source whose visible tip has not moved for `SyncDriverOptions::visible_tip_stall_limit_ms` halts the driver (`SyncDriverPhase::SourceHalted`), and new spends fail with `WalletError::ChainSourceHalted` until the tip moves. `WalletEvent::SourceHalted` and `WalletEvent::SourceRecovered` report entering and leaving the halt.
- **SYNC-14**: With `WalletOptions::minimum_source_reorg_window_blocks` set (1,000 by default, the deepest reorg Zebra produces), `Wallet::sync` and `Wallet::scan_queued_range` read `ChainSource::reorg_window_blocks` right before a SYNC-10 verdict would record a broadcast unmineable, and record nothing unless the read meets the minimum, so no key is freed and no hold reopened; a failed read holds the verdict too. `SyncDriver` also reads the window when it starts, each time it reopens the chain-event stream, and on every polling wakeup until a read succeeds. New spends fail with `WalletError::SourceReorgWindowUnverified` while a driver has not read the window, and with `WalletError::SourceReorgWindowTooShallow` while the last read showed a smaller one. `WalletEvent::SourceReorgWindowTooShallow` and `WalletEvent::SourceReorgWindowSufficient` mark a shortfall's start and end. Scanning continues, and closing the driver lifts its spend refusal. See [ADR-0005](../adrs/0005-wallet-sync-resume-anchor-cadence.md) decision 12.

### Spending (SPEND)

- **SPEND-1**: `Wallet::send_payment(SendPaymentPlan) -> SendOutcome`. Transparent and shielded recipients both supported.
- **SPEND-2**: Refuse memo on transparent recipient (ZIP-302) at API call, including a Unified Address whose only receiver is transparent; typed error variant.
- **SPEND-3**: TEX address support per ZIP-320: model TEX recipients distinctly and reject memos as transparent recipients.
- **SPEND-4**: Conventional fee per ZIP-317.
- **SPEND-5**: `nExpiryHeight` set per ZIP-203.
- **SPEND-6**: ZIP-321 payment URI parsing through `PaymentRequest::from_uri`.
- **SPEND-7**: `Wallet::shield_transparent_funds(ShieldTransparentPlan) -> SendOutcome` explicitly shields wallet-owned transparent UTXOs before shielded spending. `ShieldTransparentPlan::with_destination_pool(ShieldedPool)` selects Sapling, Orchard, or Ironwood explicitly; omitting it preserves the activation-aware default. Proposal inputs are locked through librustzcash's `LockRequest` until transaction creation records their spends. A failed build releases its locks before returning; concurrent proposals exclude active locks through `LockedInputPolicy::Exclude`.
- **SPEND-8**: `Wallet::get_account_balance(account_id) -> AccountBalance` returns a per-pool balance snapshot (Sapling, Orchard, Ironwood, transparent-mature, transparent-immature) anchored to the wallet's persisted visible tip. Read-only; composes the persisted wallet rows that drive `Wallet::sync` and `Wallet::list_unspent_shielded_notes`. Transparent values split by ZIP-213 coinbase maturity (100 confirmations) computed against `as_of_height + 1`, matching `zcash_client_backend`'s `chain_tip + 1` convention.
- **SPEND-9**: `Wallet::get_pending_transparent_inputs(account_id) -> PendingTransparentInputs` returns transparent inputs recorded for wallet-owned broadcasts that SYNC-10 has not settled. An input broadcast longer ago than `WalletOptions::pending_broadcast_window_ms` is flagged `is_stale` for the operator and is never dropped for its age. librustzcash's persisted spend and transaction-expiry state remains the input-selection authority.
- **SPEND-10**: `Wallet::send_payment` uses the same upstream proposal-lock lifecycle as SPEND-7. Once transaction creation persists the spend, librustzcash excludes the inputs while the unmined transaction remains unexpired. Zally records broadcast inputs in its operator ledger, together with the idempotency binding (CORE-5), before calling `Submitter::submit`, closing the submit-accept persistence window without maintaining a parallel input selector.
- **SPEND-11**: `Wallet::reserve_for_dispense(ReserveForDispensePlan) -> Hold` atomically locks `amount_zat` of the account's shielded balance against the caller-supplied `request_id`. The hold persists in storage so it survives a process restart; concurrent reservations whose amounts sum above spendable cannot both pass (one returns `InsufficientBalance`). The wallet plane exposes `Wallet::release_hold(hold_id)`, `Wallet::finalize_hold(hold_id, tx_id)`, and `Wallet::spendable_for_next_dispense` to complete the lifecycle; the latter subtracts every active hold from the wallet's shielded view. A hold finalized with a transaction the SYNC-10 verdict records as unmineable becomes active again, because its dispense did not happen, and `WalletEvent::HoldDispenseUnmineable` reports it; a hold whose request identifier another active hold already holds is released instead. See [ADR-0003](../adrs/0003-dispense-reservations.md).
- **SPEND-12**: Once NU7 is active at a proposal's target height, every proposal step is checked against the ZIP 218 per-block shielded limits before its inputs are used: 330 Orchard actions, 330 Ironwood actions, 300 Sapling spends and outputs, and 330 across those pools. An over-limit proposal fails with `ProposalRejectionKind::ShieldedLimitExceeded` and releases its input locks. The check is Zally's until librustzcash ships its own.

### PCZT and external signing (PCZT)

- **PCZT-1**: `Wallet::propose_pczt(plan) -> PcztBytes`: build an unsigned PCZT without holding spending keys.
- **PCZT-2**: PCZT serialization parses v1 and v2 and emits the minimal version that can represent the transaction, for export to an HSM, FROST coordinator, or air-gapped signer. `Capability::PcztV1AndV2` advertises this contract.
- **PCZT-3**: `Wallet::prove_pczt(&pczt) -> PcztBytes`: create required Sapling, Orchard, and Ironwood proofs for the in-process path while the caller retains the source artifact for failure recovery.
- **PCZT-4**: `Wallet::sign_pczt(&pczt) -> PcztBytes`: sign in-process (uses USK) while the caller retains the source artifact for failure recovery.
- **PCZT-5**: `Wallet::extract_and_submit_pczt(&final, submitter) -> SendOutcome`: extract the transaction from a fully-authorized PCZT and submit.
- **PCZT-6**: PCZT support covers transparent, Sapling, Orchard, and Ironwood bundles.
- **PCZT-7**: Wallet runtimes that return `SignedPayload` for Zcash x402 exact set `format = pczt-v2-extractable` and put extractor-ready ZIP-374 PCZT bytes in `bytes`; facilitators never need wallet private keys to verify, extract, and broadcast.
- **PCZT-8**: Successful finalized-PCZT extraction retains the exact PCZT bytes by transaction ID before submission. The retained bytes are privacy-sensitive disclosure source material: they are never logged, identical writes are idempotent, and conflicting bytes fail closed.
- **PCZT-9**: Every Zally-created PCZT carries an opaque, non-secret librustzcash lock-owner token while Zally retains the exact selected output references in its wallet ledger. `Wallet::abandon_pczt(&pczt)` releases only that artifact's inputs and is idempotent; successful extraction deletes the lifecycle row after upstream transaction storage clears the locks. If an async caller is cancelled before accepting a newly created PCZT, the storage actor performs the same owner-scoped release.
- **PCZT-10**: `Wallet::prove_pczt`, `Wallet::sign_pczt`, `Wallet::extract_pczt`, and `Wallet::extract_and_submit_pczt` judge the PCZT with CHAIN-7 at the wallet's recorded visible tip before touching the seed or storing a transaction. A PCZT that can never be mined, such as one created before NU7 activated, fails with `WalletError::TransactionUnmineable` after its inputs are released as by `Wallet::abandon_pczt`; signing it again cannot repair it, so the caller recreates it. The verdict is safe only while the wallet is the PCZT's sole broadcaster: a copy extracted and broadcast elsewhere, for example by a facilitator, may already be mined, and recreating its payment would pay twice.

### Payment disclosures (DISCLOSURE)

- **DISCLOSURE-1**: `zcash-payment-disclosure` owns the Zally-independent Draft1 and Zally Ironwood codecs, producers, and verifier. It has no dependency on a `zally-*` crate and is the boundary intended for upstream porting.
- **DISCLOSURE-2**: `Wallet::export_payment_disclosure(ExportPaymentDisclosurePlan) -> PaymentDisclosure` produces a network-tagged disclosure from a retained finalized PCZT. The wallet validates the recipient network and retained source before unsealing its seed.
- **DISCLOSURE-3**: Draft1 proves every Sapling spend, selects exactly one Sapling output by recipient and `amount_zat`, and rejects transparent inputs, Orchard actions, Ironwood actions, ZIP-304 address proofs, missing outgoing cipher keys, and ambiguous output matches.
- **DISCLOSURE-4**: `verify_disclosure` verifies the transaction ID, fresh Sapling spend proof and authorization signature against the on-chain anchor and nullifier, and output recovery with the disclosed outgoing cipher key at the supplied mined height.
- **DISCLOSURE-5**: Draft1 is an explicit experimental encoding while ZIP-311 leaves encoding and versioning unspecified. Unknown profiles fail closed; a future standardized encoding receives a different profile.
- **DISCLOSURE-6**: `ZallyIronwood` is the explicitly nonstandard profile byte `0x02`. It signs the network-bound disclosure digest with each real mined action's randomized RedPallas spend key and recovers the selected output with its outgoing cipher key. The mined transaction's consensus proof supplies the nullifier-to-key binding; dummy padding actions are excluded. It is never labeled as ZIP-311 compliance.

### Observability (OBS)

- **OBS-1**: explicit `tracing` events at meaningful state transitions with documented event names and fields. No blanket method-entry instrumentation.
- **OBS-2**: `Wallet::metrics_snapshot() -> WalletMetrics` returns a typed metrics snapshot derived from the same persisted progress as `WalletStatus`.
- **OBS-3**: `WalletEvent` async stream documented as the canonical push notification channel for state changes.
- **OBS-4**: `SyncHandle::observe_status() -> SyncSnapshotStream` is the canonical push channel for sync-driver lifecycle state.
- **OBS-5**: `Wallet::get_pending_transparent_inputs` is the operator-facing view of transparent inputs attached to recent wallet-owned broadcasts. Upstream proposal locks and persisted-spend exclusion remain internal storage invariants rather than a second Zally event stream.

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
- **ZIP-311**: Draft1 Sapling payment-disclosure production and verification. The proof algorithms follow the Draft ZIP; the explicitly versioned wire encoding is incubating because the ZIP has not specified one.
- **ZIP-315**: Wallet best practices. Explicit transparent shielding, trusted vs untrusted TXO accounting, confirmation-depth defaults aligned with the ZIP.
- **ZIP-316**: Unified Addresses, UFVKs, UIVKs. First-class address type; UFVK export.
- **ZIP-218**: Per-block shielded limits after NU7. Proposals whose steps exceed a limit are refused before they are built (SPEND-12).
- **ZIP-317**: Conventional fee mechanism.
- **ZIP-320**: TEX addresses. Recognised as transparent recipients at the API boundary.
- **ZIP-321**: Payment request URIs. First-class `PaymentRequest` type.
- **ZIP-401**: Mempool DoS protection. Submission retries respect mempool eviction.

### Reserved By Shape

- **ZIP-230**: v6 transaction format. Activates at NU6.3 for the Ironwood pool, ahead of ZIP-230's own drafted NU7/OrchardZSA scope. Transaction construction stays behind wallet/storage methods so a v6 path does not leak librustzcash internals.
- **ZIP-231**: Memo bundles. Memo API takes an opaque `Memo` value rather than exposing a Zally-owned memo enum.
- **ZIP-312**: FROST spend authorisation. PCZT signing stays in `zally-pczt`, separate from wallet sync and storage.

### Out of scope

- **ZIP-48**: Transparent multisig wallets.
- **ZIP-211**: Sprout pool sunset. Sprout is dead; Zally never deposits to Sprout. Sprout sweeps are out of scope.
- **ZIP-308**: Sprout-to-Sapling migration (see ZIP-211).

## Naming Priority

Priority order: specificity, vocabulary match, then brevity. Use the domain verb table and unit suffix rules before introducing a new naming pattern.
