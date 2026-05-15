# ADR-0001: Workspace Crate Boundaries

| Field | Value |
|-------|-------|
| Status | Accepted |
| Product | Zally |
| Domain | Workspace structure, crate dependency graph |
| Related | [Public interfaces](../architecture/public-interfaces.md) |

## Context

Zally is a Rust workspace. Crate boundaries are architecture: they enforce module boundaries the compiler checks, allow independent semver, and shape what an operator sees when they read `Cargo.toml` to decide what to depend on.

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

### Why the scan loop is not its own crate

Walking the codebase-structure rule "extend, don't split":

- The scan loop is *orchestration of `zally-chain` plus `zally-storage`*. It is not an independent domain.
- It has no independent consumer. Only `zally-wallet` uses it.
- It does not own a framework boundary. It is a tokio task driven by `zally-wallet`.
- It does not have independent change axes. When the wallet's spending API changes, the scan loop's interaction with storage changes too.

A separate crate would be ceremony, not boundary. `zally-wallet::sync` is the right shape.

### Why `zally-pczt` is a separate crate, not a module

PCZT (Partially Created Zcash Transaction) is the canonical architectural seam between *proposing* a Zcash transaction and *signing* it. The role that builds a PCZT does not need spending keys; the role that signs one does not need chain reads, storage, or send orchestration. Today both roles live in `zally-wallet` (the in-process signing path), but the seam is real and points at two product shapes:

- **Wallet services** (faucets, exchanges, payment processors, mining pools): build PCZTs, sign them in-process, submit. Depend on `zally-wallet` (which transitively pulls `zally-pczt`).
- **Signer-only services** (HSM bridges, FROST coordinators, air-gapped signers, custody backends with split key holding): receive PCZTs, sign with operator-held keys, return signed PCZTs. Depend on `zally-pczt` plus `zally-keys` only, never on `zally-wallet`.

If `zally-pczt` were a module inside `zally-wallet`, signer-only consumers would pull all of `zally-wallet`, including `tonic`, `rusqlite`, and the send-flow surface they do not use. Keeping `zally-pczt` as a peer crate gives signer-only integrations a focused dependency.

### Why no `zally-runtime` or `zally-server`

Zally is library-shaped. A `zally-runtime` crate would invite a future `zally-serve` or `zally-daemon`, which is a different product shape. Operators who want a daemon use Zallet; operators who want a library use Zally.

### Why `zally-testkit` is a peer, not a feature flag

The pattern of "test fixtures behind a `#[cfg(test)]` module" works for crate-internal tests but not for downstream crates that want to depend on Zally's fixtures. A peer crate gated by an opt-in feature flag (`zally-testkit = { version = "0.1", optional = true }`) lets consumers share Zally's fixtures without including them in production binaries.

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

`zally-core` has no internal dependencies. Every other crate depends on it directly. `zally-wallet` is the umbrella; an operator integrating Zally adds `zally-wallet` to their `Cargo.toml`, which transitively pulls the rest.

The graph is a tree, not a DAG with shortcuts. `zally-storage` does not depend on `zally-chain`; `zally-chain` does not depend on `zally-storage`; both depend only on `zally-core`. Orchestration is `zally-wallet`'s job.

## Naming discipline

Per [Public interfaces](../architecture/public-interfaces.md):

- Crate names: `zally-<noun>`. No `zally-<verb>`.
- No generic-bucket crate names. No `zally-utils`, `zally-helpers`, `zally-common`, `zally-shared`, `zally-misc`.
- No implementation-leak crate names. No `zally-sqlite`, `zally-zinder`, `zally-age`. Implementation choices are types within the appropriate domain crate.

## Consequences

**Clear dependency direction.** Seven crates is a substantial workspace; an extra orchestration crate would have been one too many.

**`zally-wallet` is larger than the other crates.** This is correct: the wallet is the umbrella that orchestrates the rest. Crate size is not a structural concern (the codebase-structure guard names this anti-pattern explicitly). The sub-modules within `zally-wallet` (`sync`, `spend`, `pczt`, `wallet`) each own their domain; nothing requires them to be separate crates.

**`zally-testkit` stays one fixture crate.** The exported fixtures are small, cohesive, and test-only.
