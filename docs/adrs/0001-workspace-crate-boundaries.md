# ADR-0001: Workspace Crate Boundaries

| Field | Value |
|-------|-------|
| Status | Accepted |
| Product | Zally |
| Domain | Workspace structure, crate dependency graph |
| Related | [Public interfaces](../architecture/public-interfaces.md) |

## Context

Zally is a Rust workspace. Crate boundaries define compile-time ownership, public dependencies, and operator-facing integration surfaces.

## Decision

Zally ships seven crates. The scan loop is a module inside `zally-wallet`, not a separate crate.

### The seven crates

| Crate | Role | Depends on |
|-------|------|------------|
| `zally-core` | Domain types: `Network`, `Zatoshis`, `BlockHeight`, `BranchId`, `TxId`, `AccountId`, `ReceiverPurpose`, `IdempotencyKey`, `Memo`, error enums. Zero domain-foreign dependencies. | (none) |
| `zally-keys` | Seed lifecycle: derivation under ZIP-32, `SeedSealing` trait with `AgeFileSealing` default, USK/UFVK/UIVK material handling, zeroization discipline. | `zally-core`, `zcash_keys`, `secrecy`, `zeroize`, `age` |
| `zally-storage` | `WalletStorage` trait wrapping librustzcash's `WalletRead`/`WalletWrite`/`WalletCommitmentTrees`/`InputSource`. Default `SqliteWalletStorage` implementation over `zcash_client_sqlite`. | `zally-core`, `zcash_client_backend`, `zcash_client_sqlite` |
| `zally-chain` | `ChainSource` trait (compact block reads, tree state, transparent UTXOs, transaction lookup, mempool peek) and `Submitter` trait (transaction broadcast). Pluggable; a `ZinderChainSource` and `ZinderSubmitter` are available behind the `zinder` feature. | `zally-core`, `zinder-client` (optional), `tonic`, `async-trait` |
| `zally-pczt` | PCZT roles for external signing and proving: `Creator`, `Prover`, `Signer`, `Combiner`, `Extractor`. Wraps `pczt` crate; adds Zally-typed error vocabulary. | `zally-core`, `zally-keys`, `pczt`, `zcash_client_backend` |
| `zally-wallet` | High-level operator API: `Wallet` handle, `Wallet::open`, `Wallet::send_payment`, `Wallet::propose_pczt`, `Wallet::prove_pczt`, `Wallet::sign_pczt`, `Wallet::observe`, `Wallet::sync`, `Wallet::capabilities`. Includes the scan-loop module orchestrating `ChainSource` and `WalletStorage`. | all the above |
| `zally-testkit` | Fixtures, mock `ChainSource`, mock `Submitter`, in-memory seed sealing, live-test gates, temp wallet paths, and regtest harness. Behind a feature flag so it never lands in operator binaries. | `zally-core`, `zally-chain`, `zally-keys` |

### Scan loop placement

The scan loop is `zally-wallet::sync`. It orchestrates `zally-chain` and `zally-storage` for the `Wallet` API, shares the wallet change axis, and has no independent public consumer.

### PCZT crate boundary

PCZT (Partially Created Zcash Transaction) is the boundary between transaction proposal and signing. The role that builds a PCZT does not need spending keys; the role that signs one does not need chain reads, storage, or send orchestration.

- **Wallet services** (faucets, exchanges, payment processors, mining pools): build PCZTs, sign them in-process, submit. Depend on `zally-wallet` (which transitively pulls `zally-pczt`).
- **Signer-only services** (HSM bridges, FROST coordinators, air-gapped signers, custody backends with split key holding): receive PCZTs, sign with operator-held keys, return signed PCZTs. Depend on `zally-pczt` plus `zally-keys` only, never on `zally-wallet`.

`zally-pczt` is a peer crate so signer-only integrations can depend on PCZT and key material without importing wallet sync, storage, chain clients, or send orchestration.

### Runtime boundary

Zally is library-shaped. It has no runtime crate, daemon crate, RPC listener, process supervisor, or package format.

### Testkit boundary

`zally-testkit` is a peer crate for downstream integration tests. Production binaries keep it out of their dependency graph by making it an optional dev/test dependency.

## Dependency graph

```
zally-core
   ▲
   ├──────── zally-keys
   │            ▲
   │            │
   ├──────── zally-storage
   │            ▲
   │            │
   ├──────── zally-chain
   │            ▲
   │            │
   ├──────── zally-pczt
   │            ▲
   │            │
   └──────── zally-wallet ◄──── operator integration

   zally-testkit ──── (feature-gated; provides mocks and live-test fixtures)
```

`zally-core` has no internal dependencies. Every other crate depends on it directly. `zally-wallet` is the umbrella crate for operator integrations.

The graph is a tree, not a DAG with shortcuts. `zally-storage` does not depend on `zally-chain`; `zally-chain` does not depend on `zally-storage`; both depend only on `zally-core`. Orchestration is `zally-wallet`'s job.

## Naming discipline

Per [Public interfaces](../architecture/public-interfaces.md):

- Crate names: `zally-<noun>`. No `zally-<verb>`.
- No generic-bucket crate names. No `zally-utils`, `zally-helpers`, `zally-common`, `zally-shared`, `zally-misc`.
- No implementation-leak crate names. No `zally-sqlite`, `zally-zinder`, `zally-age`. Implementation choices are types within the appropriate domain crate.

## Consequences

`zally-wallet` owns orchestration. `zally-chain`, `zally-storage`, `zally-keys`, and `zally-pczt` own focused boundaries. `zally-testkit` owns test fixtures and live-test gates.
