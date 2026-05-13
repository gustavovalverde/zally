# Product Requirements: Zally — Typed Rust Library for Headless Server-Side Zcash Wallets

| Field | Value |
|-------|-------|
| Status | Draft |
| Created | 2026-05-12 |
| Product | Zally |
| Audience | Operators building server-side Zcash applications (faucets, exchanges, payment processors, custody backends, mining pools); Rust developers integrating Zcash payments into existing systems; LLM agents writing or maintaining code against Zcash wallet APIs |
| Related | [librustzcash](https://github.com/zcash/librustzcash) (cryptographic and wallet-state foundation), [Zallet](https://github.com/zcash/wallet) (sibling product: JSON-RPC wallet daemon), [Zinder](https://github.com/gustavovalverde/zinder) (chain-read plane), [Zcash ZIPs](https://github.com/zcash/zips) (protocol corpus) |

## Problem Statement

The Zcash ecosystem has a complete substrate of wallet primitives in librustzcash (`zcash_client_backend`, `zcash_client_sqlite`, `zcash_keys`, `zcash_primitives`, `pczt`, `zcash_address`, `zip321`) and a clear architectural commitment that wallets, not indexers, hold keys. What it does not have is a product shape that fits the operators who want to integrate Zcash into a server. Every existing artifact lands somewhere else.

Zashi and Zodl are mobile wallets with their cryptography embedded behind a JNI shim, a Kotlin lifecycle, and an Android battery model — none of which fit a server. Zingolib is a wallet-data-file client tuned for a single human user. Zallet is a JSON-RPC daemon whose maintainers state plainly that "Zallet is not designed to be used as a Rust library; we give no guarantees about any such usage." `zcash-devtool` is a CLI of one-shot subcommands whose README declares it explicitly unsafe for production. librustzcash itself ships the data API traits but no end-to-end server example — the closest reference in the workspace is a 72-line viewing-key-to-address utility.

The result: every team building a Zcash custody backend, exchange withdrawal worker, payment processor, mining-pool payout engine, or testnet faucet either reinvents wallet logic on top of `zcash_client_backend` (and gets the dozen non-trivial decisions wrong in subtle ways), or wraps Zallet's RPC and inherits the daemon-shaped product premise as a tax on every operation. Both paths produce architectural entropy. Both produce work the ecosystem cannot share. The fauzec faucet's own `crates/fauzec-wallet` adapter is direct evidence of the gap: it exists because no upstream Rust library fits a programmatic operator, and it routes around Zallet's release-cycle bugs (`zcash/wallet#443`, `z_viewtransaction` broken, `z_listunspent` quirks documented in [zallet-rpc-quirks](https://github.com/gustavovalverde/fauzec/blob/main/docs/reference/zallet-rpc-quirks.md)) rather than calling librustzcash directly.

Zally fills the missing rung. It is the typed Rust library that operators link into their own server and call as in-process functions. It is the product shape Zallet explicitly does not provide.

## Solution

Zally is a Rust library (crate workspace, no daemon shell) that wraps librustzcash with an operator-shaped API: idempotent send operations, multi-receiver wallet model, pluggable chain source and transaction submission, encrypted-at-rest seed lifecycle, PCZT-first signing for HSM/multi-party custody, structured tracing observability, and async-native I/O. It treats `zcash_client_backend` as the wallet-logic foundation, `zcash_client_sqlite` as the default storage implementation, Zinder as the default chain source, and every other surface as pluggable.

The product is a library and a set of cookbook examples. There is no `zally-daemon`, no `zally rpc` CLI, no JSON-RPC compatibility layer. Operators who want a daemon use Zallet; operators who want a library use Zally. The boundary between the two is the architectural commitment, not a feature gap.

## Goals

- **A Rust library a server operator can link into their service in one afternoon**, with one cookbook example covering their use case (faucet send, exchange deposit detection, payment-processor reconciliation, custody withdrawal).
- **Compliance with the ZIPs that constrain wallet behaviour**: ZIPs 32, 200, 203, 212, 213, 225, 244, 302, 315, 316, 317, 320, 321, 401 in v1; design hooks for ZIPs 230, 231, 312 so they slot in without an API break.
- **Pluggable boundaries** for chain source, transaction submission, and storage. Default implementations target Zinder, but Zally is not a Zinder-only library; lightwalletd and Zebra-direct implementations are first-class alternatives.
- **Operator-grade key custody**: seeds encrypted at rest by default, spending keys held in memory only at signing time with explicit zeroization, optional PCZT export for external signers (HSM, FROST, air-gapped).
- **A boundary clear enough that LLM agents can navigate it**: typed errors with stable identifiers, capability descriptors on every public surface, examples directory that agents can lift verbatim, documented invariants on every public type.
- **Anti-entropy by construction**: a small, named surface that publishes semver-stable contracts even when its librustzcash dependency churns. Internal librustzcash upgrades happen behind the Zally surface, not in front of it.

## Non-Goals

This product does not become these things, ever. If a need pushes us toward any of them, the answer is "use the right tool" not "extend Zally."

- **Not a wallet daemon.** No JSON-RPC, no long-lived process semantics built in. Operators bring their own process model. Zallet exists for the daemon shape.
- **Not a UI library.** No balance formatting beyond `Zatoshis` ↔ ZEC conversion, no copy-strings, no notification helpers. UI is the operator's product surface.
- **Not a node.** Zally does not talk to Zebra directly for chain consensus. Chain data is consumed through a `ChainSource` abstraction; Zinder is the default; Zebra would be a sibling implementation only if a future Zebra change exposed a wallet-friendly read API.
- **Not a hardware-wallet driver.** Zally exposes PCZT for signer delegation; it does not bundle device drivers. Hardware-specific transport (USB, BLE, QR) lives in the operator's integration code, not in Zally.
- **Not a mobile wallet.** Mobile uses ECC's Android/iOS SDK. Zally is server-shaped; tokio-first, blocking-scan-via-`spawn_blocking`, structured tracing — none of which fit a mobile lifecycle.
- **Not multi-tenant.** One operator identity per process. Multi-tenant custody backends compose multiple Zally instances; they do not fork the wallet inside one instance.
- **Not a `zcashd`-compatibility shim.** No `z_listunspent`, no `z_sendmany`, no `getrawtransaction` proxying. Operators migrating from `zcashd` migrate to Zally's API, not Zally's emulation of `zcashd`'s API. (Zallet's RPC parity harness is the right tool for the `zcashd` migration path.)
- **Not a new cryptographic primitive.** Zally consumes librustzcash; it does not implement Sapling, Orchard, or transparent crypto. Bug fixes and protocol changes flow through librustzcash, not through Zally forks.

## User Stories

Grouped by role. Each story is a verification target for acceptance.

### Operator: a faucet (the canonical first integration)

1. As a faucet operator, I want to generate a seed once during bootstrap, encrypt it at rest, and recover the wallet on every subsequent restart, so that the wallet survives process and host restarts without operator re-entry.
2. As a faucet operator, I want my wallet to hold multiple receivers (mining coinbase output, donation receive, hot dispense, optional cold reserve) under one account, so that all inflow funds a single dispense path.
3. As a faucet operator, I want to construct and broadcast a shielded payment with a user-supplied memo in one async call, with idempotent semantics keyed by my own request identifier, so that retry on transient failure does not double-spend.
4. As a faucet operator, I want my wallet to refuse memos on transparent recipients and refuse shielded inputs when paying a TEX address, so that ZIP 320 and ZIP 302 compliance is enforced by the library rather than the integration code.
5. As a faucet operator, I want sub-block-time feedback when a transparent donation lands (mempool event, then confirm event), so that the UI reflects state without my code polling.
6. As a faucet operator, I want explicit confirmation depth policy per address kind, so that mining coinbase outputs wait the protocol-required 100 blocks and other receives wait my configured target.

### Operator: an exchange or custody backend

7. As an exchange operator, I want to generate TEX deposit addresses per user (ZIP 320), and observe per-address UTXO changes, so that deposits route to the correct customer ledger entry.
8. As a custody operator, I want to export a UFVK for an auditor without exposing spending capability, so that proof-of-reserve disclosures use the protocol's intended viewing-key separation.
9. As a custody operator, I want to delegate signing to an HSM or FROST quorum by exporting a PCZT, gathering signatures externally, and re-importing the signed transaction, so that no single process holds spend authority.
10. As an exchange withdrawal operator, I want to batch many outgoing payments into one transaction with one fee, so that exchange withdrawal economics work at scale.
11. As an exchange operator, I want every payment I send to have a stable, non-malleable txid (ZIP 244), so that my settlement system tracks payments by id without confusion.

### Operator: a payment processor

12. As a payment processor, I want to parse a `zcash:` URI (ZIP 321) into a typed payment request, validate it against my policies, and execute the payment via one library call, so that integration with merchant frontends is mechanical.
13. As a payment processor, I want to attach a merchant-supplied invoice identifier to a memo (ZIP 302 today, ZIP 231 memo bundles when ratified), so that reconciliation can match on-chain settlement to off-chain invoices without out-of-band state.
14. As a payment processor, I want every transaction to set `nExpiryHeight` (ZIP 203) appropriately and to be re-built rather than re-broadcast after expiry, so that long-running operations remain consistent under network conditions.

### Operator: a mining pool

15. As a mining pool operator, I want to recognise shielded coinbase outputs (ZIP 213) and respect the 100-block coinbase maturity rule, so that payouts cannot accidentally spend immature coinbase notes.
16. As a mining pool operator, I want to issue many small shielded payouts in parallel without per-account contention, so that batch payout windows complete on time.

### Developer: integrating Zally into existing Rust code

17. As a Rust integrator, I want one cookbook example per use case (faucet send, exchange deposit, payment processor, custody withdrawal, mining payout) that compiles, runs against regtest, and demonstrates the public API end-to-end.
18. As a Rust integrator, I want Zally's public Rust API documented with rustdoc on every method, every type, every error variant, so that `cargo doc --no-deps -p zally-wallet` is the canonical integration reference.
19. As a Rust integrator, I want stable Zally APIs across librustzcash upgrades, so that my code does not break when `zcash_client_backend 0.22` becomes `0.23`. Zally absorbs the upstream churn.
20. As a Rust integrator, I want test fixtures (mock chain source, in-memory storage, regtest harness) so that I can unit-test my application's wallet integration without a running node.

### Agent: an LLM agent integrating against Zally or maintaining code that uses Zally

21. As an agent integrator, I want every error returned by Zally to be a typed enum variant with a stable identifier and a documented retry posture, so that retry, alert, and gate decisions are mechanical and version-stable.
22. As an agent integrator, I want a `ServerInfo`-equivalent capability descriptor that I can read at runtime (e.g., "ZIP 231 memo bundles supported? Y/N", "PCZT v0.6 export supported? Y/N"), so that I detect supported features without pinning to a Zally version.
23. As an agent integrator, I want the examples directory to be the canonical reference, with each example self-contained and runnable, so that my reasoning starts from working code rather than prose.
24. As an agent integrator, I want naming conventions (`*_zat` for zatoshi amounts, `*_blocks` for confirmation counts, `_network` always passed alongside addresses) enforced by the public API, so that ambiguous integrations are not even expressible.

### End user of a consumer (donor, claimer, exchange customer)

25. As the end user of a wallet, faucet, or exchange built on Zally, I want sub-block-time feedback when a transparent transaction lands at a watched address, so that the consumer's UI updates within seconds.
26. As the end user, I want my memos to round-trip exactly as written, including UTF-8 text and structured (application-private) memos.
27. As the end user, I want my privacy preserved by default: shielded receivers preferred where possible, transparent only when the recipient or counterparty constraints require it.

## Architectural Commitments

Implementation-level decisions locked at the PRD layer because they shape every subsequent design choice. Each commitment is justified and refers to its evidence base.

1. **Library-first, no daemon shell.** Confirmed by ecosystem mapping: every existing artifact occupies a different product shape. Zally's value is precisely the missing rung. A daemon shell would compete with Zallet and dilute the library product.

2. **librustzcash as the cryptographic and wallet-state foundation.** Not a fork, not a rewrite — a versioned dependency. Zally re-exports types from `zcash_primitives`, `zcash_keys`, `zcash_address`, and `zip321` through its own surface, but the underlying crypto and state machines are librustzcash's. Bug fixes and protocol changes flow upstream.

3. **A multi-crate workspace from day one.** Following Zinder's pattern (`zinder-core`, `zinder-store`, `zinder-source`, `zinder-runtime`, etc.). Crate boundaries enforce architecture. The boundary set is locked in [ADR-0001](adrs/0001-workspace-crate-boundaries.md); the seven crates are:
   - `zally-core` — domain types (`Network`, `AccountId`, `ReceiverPurpose`, `Zatoshis`, `BlockHeight`, errors). Zero domain-foreign dependencies.
   - `zally-keys` — seed lifecycle, encryption-at-rest trait (`SeedSealing`), USK/UFVK/UIVK derivation, zeroization discipline.
   - `zally-storage` — `WalletStorage` trait (Zally's typed wrapper over `WalletRead`/`WalletWrite`), default `SqliteWalletStorage` impl on `zcash_client_sqlite`.
   - `zally-chain` — `ChainSource` (block reads, tree state, transaction lookups) and `Submitter` (transaction broadcast) traits; default `ZinderChainSource`, alternative `LightwalletdChainSource`.
   - `zally-pczt` — typed PCZT roles (`Creator`, `Signer`, `Combiner`, `Extractor`) for HSM and multi-party flows.
   - `zally-wallet` — high-level operator API (`Wallet::send`, `Wallet::observe_receiver`, `Wallet::export_ufvk`, `Wallet::propose_pczt`). Includes the scan-loop module orchestrating `ChainSource` + `WalletStorage`. The umbrella crate operators link.
   - `zally-testkit` — fixtures, mock chain sources, in-memory storage, regtest helpers. Behind a feature flag so it never lands in operator binaries.

   The scan loop is intentionally not its own crate. It is orchestration of `zally-chain` + `zally-storage`, which is exactly what `zally-wallet` exists for. ADR-0001 records the reasoning.

4. **Async, tokio-first.** Every public I/O-touching method returns a `Future`. The librustzcash scan loop is blocking; Zally wraps it in `tokio::task::spawn_blocking`. Sync APIs are not exposed even where they would compile. Justification: server consumers are async by default; offering a sync API invites them to hold the lock across awaits.

5. **Trait-abstracted boundaries.** The three external dependencies — chain source, transaction submission, storage — are each behind a trait with a typed associated `Error`. Default implementations are first-class but swappable. Justification: operators run different topologies (Zinder, lightwalletd-pinned legacy, future Zinder-derived networks). Trait abstraction is the anti-entropy move that lets every consumer reuse Zally regardless of their chain plane choice.

6. **PCZT is first-class, not bolted on.** Zally exposes both an in-process signing path (default, simple) and a PCZT export path (for HSMs, FROST, air-gapped operator workflows). Justification: server consumers include custody backends where in-process signing is operationally unacceptable. PCZT support cannot be a v2 retrofit; it shapes the `Wallet::propose_*` and `Wallet::sign_*` API surface.

7. **Seeds encrypted at rest by default.** `zally-keys` exposes a `SeedSealing` trait with a required default implementation (`AgeFileSealing`, age-encrypted file at a configurable path). Operators bring their own implementations for AWS KMS, GCP KMS, HashiCorp Vault, etc. Plain-text seed storage is supported only behind an explicit `unsafe_plaintext_seed` configuration flag, and the resulting `Wallet::open_with_seed_sealing(...)` log line is emitted at WARN level on every open. Justification: most existing tutorials and example code store seeds in plain text. Zally's defaults push the right direction without forcing operators to write encryption code.

8. **Idempotent send semantics.** Every `Wallet::send_*` method takes a caller-supplied `IdempotencyKey: AsRef<str>`. The wallet's storage layer records (key, txid) tuples. Retrying a send with the same key returns the prior txid rather than re-broadcasting. Justification: server consumers retry on transient failure; without idempotency, retries are double-spends.

9. **Strict ZIP enforcement at the API boundary.** Refuse memo on transparent recipient (ZIP 302). Refuse shielded input when paying TEX address (ZIP 320). Refuse spend of coinbase note below maturity (ZIP 213). Refuse mainnet operations without an explicit `--mainnet` feature flag or builder call (defensive default — testnet/regtest first-class, mainnet opt-in).

10. **Network-tagged everything.** Every public type that names an address, key, balance, or transaction carries a `Network` value or is generic over a `NetworkSpec` parameter. Functions that take an address but not a network are a compile error. Justification: the most catastrophic class of wallet bug is signing a testnet transaction with mainnet keys (or vice versa). Make the bug unrepresentable.

11. **Errors are typed enums per boundary.** No `Box<dyn Error>`, no `anyhow`, no `Other(String)` catch-alls in any public Zally surface. `thiserror` v2 throughout. Each error has a documented retry posture (`retryable`, `not_retryable`, `requires_operator`). Justification: agent experience — mechanical retry decisions.

12. **No `unsafe`, no `unwrap`, no `expect`, no `panic`** in any Zally crate. Workspace lints (inherited from Zinder) enforce. Justification: a wallet that panics on malformed chain data is a vulnerability.

13. **Capability advertisement.** `zally-wallet` exposes a `Wallet::capabilities() -> WalletCapabilities` method returning a typed descriptor (supported pools, PCZT version, ZIP 231 readiness, sealing implementation, network). Agents and integrators read this at runtime. Justification: ZIPs evolve; consumers stay compatible by feature detection, not version pinning.

14. **Observability is structured tracing, not logs.** Every operation emits a `tracing` span with documented fields. Metrics (scan progress, balance per pool, pending tx count, broadcast errors) exposed via `Wallet::metrics_snapshot()` for operators to pull into Prometheus through their own adapter. Justification: server consumers need pluggable observability; Zally cannot bake in one metrics backend.

15. **`zcash_client_sqlite` as the default storage backend, behind a trait.** Operators who need libSQL/Turso for multi-region read replicas, or PostgreSQL for unified ops infrastructure, implement `WalletStorage` themselves. The trait surface is small enough to be re-implementable; `zcash_client_sqlite` is large enough to satisfy 90% of consumers out of the box.

16. **Documentation as a deliverable, not an afterthought.** Every public type, function, error variant, and trait method has a rustdoc block with: contract (what it promises), failure modes (what errors it returns and when), example (one line minimum, full example for non-trivial), invariants. `RUSTDOCFLAGS='-D warnings'` enforced in CI.

## Capability Requirements

Numbered for traceability. Each is a binary acceptance gate for v1.

### Wallet core (REQ-CORE)

- **REQ-CORE-1**: Generate a fresh wallet (mnemonic + ZIP-32 derivation) for any network. Single async call.
- **REQ-CORE-2**: Open an existing wallet from a sealed seed; fail closed on integrity error.
- **REQ-CORE-3**: Multi-receiver model: one wallet hosts named receivers (`mining`, `donations`, `hot_dispense`, `cold_reserve`, operator-defined). Each receiver has a derived address, a purpose, and observable balance.
- **REQ-CORE-4**: UA derivation per ZIP 316; UFVK and UIVK export.
- **REQ-CORE-5**: Idempotent send by `IdempotencyKey`.
- **REQ-CORE-6**: Network-tagged types throughout; compile error if a network is missing.

### Key management (REQ-KEYS)

- **REQ-KEYS-1**: `SeedSealing` trait with `AgeFileSealing` as default.
- **REQ-KEYS-2**: `unsafe_plaintext_seed` opt-in with `WARN`-level log on every open.
- **REQ-KEYS-3**: USK derived from seed at signing time; zeroized after use.
- **REQ-KEYS-4**: UFVK / UIVK export with stable serialization; spending key never serialized through Zally's public API.
- **REQ-KEYS-5**: ZIP 325 (account metadata keys) derivation hook for v2; out-of-scope features documented in the trait.

### Chain source and submission (REQ-CHAIN)

- **REQ-CHAIN-1**: `ChainSource` trait with methods for compact-block range fetch, latest tree state, transaction lookup, address-UTXO read, mempool peek.
- **REQ-CHAIN-2**: `Submitter` trait with single `submit(raw_tx, Network) -> SubmitResult` method.
- **REQ-CHAIN-3**: Default `ZinderChainSource` implementation against `zinder-client::ChainIndex`.
- **REQ-CHAIN-4**: Alternative `LightwalletdChainSource` for legacy operator topologies.
- **REQ-CHAIN-5**: Reconnect, retry, and circuit-breaker logic at the trait boundary, not duplicated in each implementation.

### Sync and scanning (REQ-SYNC)

- **REQ-SYNC-1**: Catch up to chain tip on `Wallet::sync(...)`.
- **REQ-SYNC-2**: Incremental sync emits `tracing` events with scan progress and per-pool note counts.
- **REQ-SYNC-3**: Reorg handling: on continuity error, automatic rollback to the longest common prefix; emit `WalletEvent::ReorgDetected`.
- **REQ-SYNC-4**: Configurable confirmation depth per receiver purpose (default 1 for non-coinbase, mandatory 100 for coinbase per ZIP 213).
- **REQ-SYNC-5**: `WalletEvent` async stream for `ReceiverObserved`, `TransactionConfirmed`, `ReorgDetected`, `ScanProgress`.

### Spending (REQ-SPEND)

- **REQ-SPEND-1**: `Wallet::send(IdempotencyKey, Recipient, Amount, Memo, FeeStrategy) -> SendResult`. Transparent and shielded recipients both supported.
- **REQ-SPEND-2**: Refuse memo on transparent recipient (ZIP 302) at API call; typed error variant.
- **REQ-SPEND-3**: TEX address support per ZIP 320: refuse shielded inputs when paying TEX recipient.
- **REQ-SPEND-4**: Conventional fee per ZIP 317 by default; `FeeStrategy::Custom(Zatoshis)` for advanced operators.
- **REQ-SPEND-5**: `nExpiryHeight` set per ZIP 203; expired transactions automatically rebuilt on retry.
- **REQ-SPEND-6**: Batch send: `Wallet::send_batch(Vec<Payment>)` builds one transaction with one fee.
- **REQ-SPEND-7**: ZIP 321 payment URI parsing: `PaymentRequest::from_uri(&str)` and `Wallet::send_payment_request(request)`.

### PCZT and external signing (REQ-PCZT)

- **REQ-PCZT-1**: `Wallet::propose_pczt(payment) -> Pczt` — build an unsigned PCZT without holding spending keys.
- **REQ-PCZT-2**: PCZT serialization to bytes for export to HSM, FROST coordinator, air-gapped signer.
- **REQ-PCZT-3**: `Wallet::sign_pczt(pczt) -> SignedPczt` — sign in-process (uses USK).
- **REQ-PCZT-4**: `Wallet::extract_and_submit_pczt(signed_pczt) -> SubmitResult` — extract the transaction from a fully-signed PCZT and submit.
- **REQ-PCZT-5**: PCZT support covers transparent, Sapling, and Orchard bundles (matching `pczt v0.6.0` capabilities).

### Observability (REQ-OBS)

- **REQ-OBS-1**: `tracing` spans on every public async method with documented field schema.
- **REQ-OBS-2**: `Wallet::metrics_snapshot() -> WalletMetrics` returns a typed snapshot (balance per pool, scan height, pending tx count, last error per receiver).
- **REQ-OBS-3**: `WalletEvent` async stream documented as the canonical push notification channel for state changes.

### Documentation and DX (REQ-DOC)

- **REQ-DOC-1**: Every public item has rustdoc. CI enforces with `RUSTDOCFLAGS='-D warnings'`.
- **REQ-DOC-2**: `examples/` directory contains at minimum: `faucet/`, `exchange-deposit/`, `payment-processor/`, `custody-with-pczt/`, `mining-payout/`. Each example compiles, runs against regtest, and is referenced from the README cookbook section.
- **REQ-DOC-3**: A "What Zally is and is not" boundary doc at `docs/architecture/product-boundary.md` mirroring Zinder PRD-0002 REQ-16, pointing operators at Zallet for daemon needs and at ECC's SDKs for mobile.
- **REQ-DOC-4**: `docs/architecture/public-interfaces.md` is the vocabulary spine — written before any public type lands.

### Agent experience (REQ-AX)

- **REQ-AX-1**: `WalletCapabilities` descriptor exposing typed feature flags. Documented enum of capability identifiers; new capabilities are additive.
- **REQ-AX-2**: Every error variant has a documented retry posture (`retryable`, `not_retryable`, `requires_operator`). Documented in rustdoc and in `docs/reference/error-vocabulary.md`.
- **REQ-AX-3**: Examples are self-contained; each `examples/<name>/main.rs` runs end-to-end without external prerequisite shell scripts.

## ZIP Compliance Surface

Explicit list of which ZIPs Zally v1 implements, deferred-but-designed-for, and explicitly does not handle. Operators consuming Zally should read this section to know what protocol behaviour the library guarantees.

### Implemented in v1 (mandatory; gates the release)

- **ZIP 32** — Shielded HD wallets. Multi-account derivation under standard paths. Source of all keys.
- **ZIP 200** — Network upgrade mechanism. Active branch ID consulted at transaction construction time.
- **ZIP 203** — Transaction expiry. `nExpiryHeight` set on every transaction with operator-configurable window (default ~40 blocks).
- **ZIP 212** — `rseed` note plaintext encoding. Inherited from librustzcash.
- **ZIP 213** — Shielded coinbase + 100-block maturity rule. Enforced at spending input selection.
- **ZIP 225** — v5 transaction format. The default on every supported network.
- **ZIP 244** — Non-malleable TxID. Tracked in wallet state; exposed via `WalletEvent` and `SendResult`.
- **ZIP 302** — Memo encoding (current 512-byte format). Encoded and decoded per spec.
- **ZIP 315** — Wallet best practices. Auto-shielding policy, trusted vs untrusted TXO accounting, confirmation-depth defaults aligned with the ZIP.
- **ZIP 316** — Unified Addresses, UFVKs, UIVKs. First-class address type; UFVK export.
- **ZIP 317** — Conventional fee mechanism. Default fee strategy.
- **ZIP 320** — TEX addresses. Recognised, refused-on-shielded-input enforced at spend.
- **ZIP 321** — Payment request URIs. First-class `PaymentRequest` type.
- **ZIP 401** — Mempool DoS protection. Submission retries respect mempool eviction; rebroadcast with bumped fee available on operator opt-in.

### Designed-for-forward-compatibility (API hooks; not enforced in v1)

- **ZIP 230** — v6 transaction format (NU7, OrchardZSA). Builder API structured so a v6 path lands as an additive enum variant, not a rewrite.
- **ZIP 231** — Memo bundles. Memo API takes an opaque `Memo` value; ZIP 231 memo bundle variants slot in when ratified.
- **ZIP 312** — FROST spend authorisation. PCZT signer trait designed so FROST coordinator integration lands as an alternative `Signer` implementation.

### Explicitly out of scope for v1

- **ZIP 48** — Transparent multisig wallets. Not in v1; operator-driven if needed.
- **ZIP 211** — Sprout pool sunset. Sprout is dead; Zally never deposits to Sprout. Sprout sweeps from legacy wallets are out of scope.
- **ZIP 308** — Sprout-to-Sapling migration. Out of scope (see ZIP 211).
- **ZIP 311** — Payment disclosures. Designed-for in v2; not in v1 release.

## Acceptance Criteria

Each REQ above is binary: complete or not. v1 ships when:

1. All REQ-CORE, REQ-KEYS, REQ-CHAIN, REQ-SYNC, REQ-SPEND, REQ-OBS, REQ-DOC, REQ-AX requirements are complete.
2. All v1-mandatory ZIPs in the compliance section above are exercised by at least one integration test (T1) and one live test (T3) against regtest or testnet.
3. All five cookbook examples in `examples/` build, run, and produce documented output against regtest.
4. T3 live tests pass against z3 regtest, public testnet, and (gated behind explicit feature flag) mainnet.
5. `cargo doc --no-deps --workspace --all-features` runs with `RUSTDOCFLAGS='-D warnings'`.
6. CI gate (matching Zinder's): `cargo fmt --check`, `cargo check`, `cargo clippy -- -D warnings`, `cargo nextest run --profile=ci`, `cargo nextest run --profile=ci-perf`, `cargo deny check`, `cargo machete` all green.
7. `docs/architecture/public-interfaces.md` is complete, covers every public type and naming rule, and is the spine that other docs reference.
8. At least one external operator integration (fauzec, or another ZF / community project) is built on Zally and in production.

## Risks and Trade-offs

Honest accounting of what we accept to make this work.

- **librustzcash pre-1.0 churn.** `zcash_client_backend` ships breaking changes every 1-3 months. Zally absorbs the churn so its public surface stays stable; this is engineering work and adds version-bump fatigue. Mitigation: a documented librustzcash version-pinning policy; Zally's release cadence is decoupled (Zally minor releases can pin a librustzcash major). Worst case: a librustzcash break we can't absorb forces a Zally major bump; we document the affected APIs.
- **Anchor selection without normative guidance.** ZIP 306 is a stub; Zally must pick an anchor depth without protocol guidance. Default: 10 blocks back (matching librustzcash's testkit default). Documented; operators can override.
- **PCZT format predates a ZIP.** PCZT is implemented (`pczt v0.6.0`) but lacks a normative ZIP spec. Zally's PCZT serialization follows librustzcash's; if a future ZIP differs, Zally bumps to the ZIP-compliant version.
- **No "official Zcash wallet library" position.** The ecosystem has not formally blessed any library as "the" wallet library; Zally is one entrant. Mitigation: positioning is explicit (sibling to Zallet, foundation for fauzec/similar); we don't claim to be "the" anything. Adoption is earned by being the right product.
- **Custody-grade hardening is its own product gate.** Zally v1 is operator-grade, not custody-grade. A custody backend storing significant value should treat Zally v1 as a starting point and add: HSM integration via PCZT, signing-key sharding via FROST, separate ops and signing processes, audit-grade logging. Documented as a v2 expansion track.
- **Mainnet caution gate.** v1 mainnet support exists but is opt-in. Justification: until at least one production deployment has run end-to-end on mainnet for a documented window without operator intervention, no operator should treat mainnet support as "ready by default."

## Open Questions

1. **Workspace crate boundaries.** ~~The eight-crate proposal in Architectural Commitments #3 is my best guess. Worth a brief explicit design review before code lands to confirm none collapses or splits.~~ Resolved by [ADR-0001](adrs/0001-workspace-crate-boundaries.md): seven crates, with the scan loop as a module inside `zally-wallet` rather than its own crate.
2. **Mnemonic standard.** BIP-39 is canonical; ZIP-32 derivation is what we run on top. Confirm Zally exposes BIP-39 mnemonic generation/recovery as the operator interface, or whether we expose a raw seed bytes interface and let operators bring their own mnemonic library.
3. ~~**Storage backend abstraction depth.** `WalletStorage` as a thin Zally trait over `zcash_client_sqlite`, or `WalletStorage` as a thicker abstraction that hides librustzcash's `WalletWrite` entirely?~~ Resolved by [ADR-0002](adrs/0002-founding-implementation-patterns.md) Decision 2: medium-thick. Zally-owned verbs on the trait surface; librustzcash signatures never appear in `pub` API.
4. **Sealing trait granularity.** `SeedSealing` covers the seed; do we also abstract USK at-rest sealing (relevant only if USK is cached beyond a single signing operation, which v1 does not do)? Default lean: no, USK is ephemeral; revisit if cache-at-rest becomes useful.
5. **Async runtime portability.** Tokio-first per Architectural Commitments #4; do we also support smol or async-std via a runtime-agnostic trait surface? Default lean: tokio-only for v1; the only known consumers (fauzec, anticipated faucets/exchanges) all use tokio.
6. **Mainnet release gate.** What constitutes "ready for mainnet"? Concrete proposal: 30 days of production operation by at least one operator, no critical issues in the period, security review by a qualified party. Decision is for the maintainers' release-planning ADR.
7. **Capability identifier vocabulary.** REQ-AX-1 mandates a typed capability descriptor; the initial vocabulary needs to be defined. Default lean: enum identifiers matching ZIP numbers where applicable (`Zip231MemoBundles`, `Zip312Frost`, `PcztV06`), plus Zally-specific capability strings (`SealingAgeFile`, `SubmitterZinder`). Refined in `docs/reference/capability-vocabulary.md`.
8. **Relationship to Zinder PRD-0002.** Several Zally requirements (typed error vocabulary, capability descriptors, push-style notifications) parallel Zinder PRD-0002 requirements. Worth aligning vocabulary between the two repos so operators integrating both see a consistent surface.

## Stakeholders

- **Gustavo Valverde** (maintainer): ratify, sequence, allocate maintenance.
- **Electric Coin Company wallet team**: review for alignment with their librustzcash investment direction and the Android/iOS SDK posture.
- **Zallet maintainers**: review the boundary positioning (Zally as library sibling, Zallet as daemon) to ensure the framing accurately describes Zallet's intended consumption model.
- **librustzcash maintainers**: review the API extraction; potential upstream contributions (Zinder `BlockSource` adapter, server-wallet example) flow through them.
- **Zashi / Zodl maintainers**: review for mobile-vs-server boundary clarity.
- **Initial integrators**: fauzec is the primary first consumer; other community projects (custody, exchanges, payment processors) invited to weigh in on REQ priority and acceptance criteria.
- **External security review (future)**: cryptographic correctness of the wrapper layer, key custody discipline, transaction-construction edge cases. Required gate for the mainnet release.

## Document lifecycle

This PRD is `Draft` until v1 release planning sequences each requirement. The first ADR (ADR-0001) records the workspace crate boundaries; subsequent ADRs record each architectural commitment that needs deeper design (chain source trait shape, PCZT API surface, sealing trait contract). The PRD is edited in place for clarifications; substantive scope changes spawn a new PRD with an incremented number.

`docs/architecture/public-interfaces.md` is the next document to write — the vocabulary spine that locks naming conventions, error vocabulary, type rules, and config conventions before any public type lands. Zally inherits the spine pattern from Zinder; deviations from Zinder's conventions must be explicit and justified.
