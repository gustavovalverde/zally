# ADR-0001: Workspace Crate Boundaries

| Field | Value |
|-------|-------|
| Status | Accepted |
| Product | Zally |
| Domain | Workspace structure, crate dependency graph |
| Related | [PRD-0001](../prd-0001-zally-headless-wallet-library.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

Zally is a Rust workspace. Crate boundaries are architecture: they enforce module boundaries the compiler checks, allow independent semver, and shape what an operator sees when they read `Cargo.toml` to decide what to depend on. Getting the boundary set right at the founding is cheap; getting it wrong creates years of cleanup churn.

The PRD's Architectural Commitment #3 proposed eight crates: `zally-core`, `zally-keys`, `zally-storage`, `zally-chain`, `zally-sync`, `zally-pczt`, `zally-wallet`, `zally-testkit`. This ADR refines that proposal.

## Decision

Zally ships seven crates, not eight. `zally-sync` collapses into `zally-wallet`.

### The seven crates

| Crate | Role | Depends on |
|-------|------|------------|
| `zally-core` | Domain types: `Network`, `Zatoshis`, `BlockHeight`, `BranchId`, `TxId`, `AccountId`, `ReceiverPurpose`, `IdempotencyKey`, `Memo`, error enums. Zero domain-foreign dependencies. | (none) |
| `zally-keys` | Seed lifecycle: derivation under ZIP-32, `SeedSealing` trait with `AgeFileSealing` default, USK/UFVK/UIVK material handling, zeroization discipline. | `zally-core`, `zcash_keys`, `secrecy`, `zeroize`, `age` |
| `zally-storage` | `WalletStorage` trait wrapping librustzcash's `WalletRead`/`WalletWrite`/`WalletCommitmentTrees`/`InputSource`. Default `SqliteWalletStorage` implementation over `zcash_client_sqlite`. | `zally-core`, `zcash_client_backend`, `zcash_client_sqlite` |
| `zally-chain` | `ChainSource` trait (compact block reads, tree state, transparent UTXOs, transaction lookup, mempool peek) and `Submitter` trait (transaction broadcast). Default `ZinderChainSource`. Alternative `LightwalletdChainSource` for legacy operator topologies. | `zally-core`, `zinder-client`, `tonic`, `async-trait` |
| `zally-pczt` | PCZT roles for external signing: `Creator`, `Signer`, `Combiner`, `Extractor`. Wraps `pczt` crate; adds Zally-typed error vocabulary. | `zally-core`, `zally-keys`, `pczt`, `zcash_client_backend` |
| `zally-wallet` | High-level operator API: `Wallet` handle, `Wallet::open`, `Wallet::send`, `Wallet::propose_pczt`, `Wallet::sign_pczt`, `Wallet::observe`, `Wallet::sync`, `Wallet::export_ufvk`, `Wallet::capabilities`. Includes the scan-loop module orchestrating `ChainSource` + `WalletStorage`. | all the above |
| `zally-testkit` | Fixtures, mock `ChainSource`, mock `Submitter`, in-memory `WalletStorage`, regtest harness. Behind a feature flag so it never lands in operator binaries. | `zally-core`, `zally-storage` (mock impl), `zally-chain` (mock impl), `zcash_client_memory` |

### Why `zally-sync` collapses

The PRD proposed `zally-sync` as a separate crate for the scan-loop logic. Walking the codebase-structure rule "extend, don't split":

- The scan loop is *orchestration of `zally-chain` + `zally-storage`*. It is not an independent domain.
- It has no independent consumer. Only `zally-wallet` uses it.
- It does not own a framework boundary. It is a tokio task driven by `zally-wallet`.
- It does not have independent change axes. When the wallet's spending API changes, the scan loop's interaction with storage changes too.

A separate crate would be ceremony, not boundary. `zally-wallet::sync` (module within the wallet crate) is the right shape.

### Why `zally-pczt` is a separate crate, not a module

PCZT (Partially Created Zcash Transaction) is the canonical architectural seam between *proposing* a Zcash transaction and *signing* it. The role that builds a PCZT does not need spending keys; the role that signs one does not need chain reads, storage, or send orchestration. Today both roles live in `zally-wallet` (the in-process signing path), but the seam is real and points at two product shapes:

- **Wallet services** (faucets, exchanges, payment processors, mining pools) — build PCZTs, sign them in-process, submit. Depend on `zally-wallet` (which transitively pulls `zally-pczt`).
- **Signer-only services** (HSM bridges, FROST coordinators, air-gapped signers, custody backends with split key holding) — receive PCZTs, sign with operator-held keys, return signed PCZTs. Depend on `zally-pczt` + `zally-keys` only, never on `zally-wallet`.

By the strict reading of the codebase-structure skill, the split fails the "independent heavy consumers" rule *today*: no signer-only consumer exists in the workspace yet, so the second importer that would justify the split has not materialised. The default — "extend, don't split" — would fold `zally-pczt` into `zally-wallet::pczt`.

We deliberately deviate from the default. Two facts make this an architectural bet, not speculative splitting:

1. **The product need is concrete, not hypothetical.** PRD-0001 REQ-PCZT-1 through REQ-PCZT-5 specify operator workflows where the signer is physically separated from the proposer: HSM signing, FROST threshold signing, air-gapped operator workflows, custody backends with split key holding. These are named v1 requirements, not "maybe someone will want this someday." The question is not whether signer-only consumers will exist; it is when the first one lands and on which timeline.

2. **Asymmetric revertability is the tiebreaker.** If `zally-pczt` is a module inside `zally-wallet` and the first HSM-bridge or FROST-coordinator service materialises, that consumer must either pull all of `zally-wallet` (carrying `tonic`, `rusqlite`, and the entire send-flow surface it does not use) or coordinate an API extraction across two crates while preserving back-compat. Both are expensive. Conversely, if `zally-pczt` stays a separate crate and no signer-only consumer materialises within the validation window below, folding it back is a single PR with zero downstream consumer impact.

We accept one speculative-feeling crate now in exchange for an unambiguously cheap revert if the bet is wrong, and an unambiguously cheap consumer onboarding if the bet is right.

### Why no `zally-runtime` or `zally-server`

The PRD is explicit that Zally is library-shaped. A `zally-runtime` crate would invite a future `zally-serve` or `zally-daemon` — which is exactly Zallet's product shape, not Zally's. No service crate exists today; if a long-lived process becomes operationally useful in v2 (a sync sidecar, an observer process), it lands as a `services/` directory at that time and requires a new ADR.

### Why `zally-testkit` is a peer, not a feature flag

The pattern of "test fixtures behind a `#[cfg(test)]` module" works for crate-internal tests but not for downstream crates that want to depend on Zally's fixtures (e.g., fauzec's wallet-plane integration tests). A peer crate gated by an opt-in feature flag (`zally-testkit = { version = "0.1", optional = true }`) lets consumers share Zally's fixtures without including them in production binaries.

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

   zally-testkit ──── (feature-gated; provides mocks for the four traits above)
```

`zally-core` has no internal dependencies. Every other crate depends on it directly. `zally-wallet` is the umbrella; an operator integrating Zally adds `zally-wallet` to their `Cargo.toml`, which transitively pulls the rest.

The graph is a tree, not a DAG with shortcuts. `zally-storage` does not depend on `zally-chain`; `zally-chain` does not depend on `zally-storage`; both depend only on `zally-core`. Orchestration is `zally-wallet`'s job.

## Naming discipline

Per [Public interfaces](../architecture/public-interfaces.md):

- Crate names: `zally-<noun>`. No `zally-<verb>` (the PRD originally proposed `zally-sync` as a verb-named crate; collapsing it removes the irregularity).
- No generic-bucket crate names. No `zally-utils`, `zally-helpers`, `zally-common`, `zally-shared`, `zally-misc`.
- No implementation-leak crate names. No `zally-sqlite`, `zally-zinder`, `zally-age`. Implementation choices are types within the appropriate domain crate.

## Consequences

**Smaller surface, clearer dependency direction.** Seven crates is a substantial workspace; eight was one too many for the orchestration role.

**`zally-wallet` is larger than the other crates.** This is correct: the wallet is the umbrella that orchestrates the rest. Crate size is not a structural concern (the codebase-structure guard names this anti-pattern explicitly). The sub-modules within `zally-wallet` (`sync`, `send`, `observe`, `open`) will each own their domain; nothing requires them to be separate crates.

**No path to splitting `zally-testkit` into per-trait fixture crates.** If `zally-testkit` becomes large enough that consumers want to pull only the chain mock or only the storage mock, we revisit. v1 ships one fixture crate.

**Empty crates are not pre-created.** The seven crates listed above are the planned set; each lands when its first real code does. Empty `Cargo.toml`s and `lib.rs` files would be ceremony.

## Migration

Existing PRD references to `zally-sync` are updated to "the `sync` module within `zally-wallet`" in the next PRD edit. No code exists yet, so no other migration is required.

## Open questions

1. **Validating the `zally-pczt` bet.** The seven-crate boundary set treats PCZT as an architectural seam ahead of an external consumer materialising (see "Why `zally-pczt` is a separate crate, not a module" above). The bet is *validated* when at least one signer-only consumer — HSM bridge, FROST coordinator, air-gapped signer workflow, or custody backend with split key holding — builds against `zally-pczt` without depending on `zally-wallet`. The bet is *invalidated* if no such consumer materialises within 12 months of Zally v1.0 shipping. On invalidation, a follow-up ADR records the collapse of `zally-pczt` into `zally-wallet::pczt`; the trait surface remains the same, only the crate boundary moves. This question is the explicit revisit point; a reviewer reading the ADR in 2027 should find either a validated bet (signer-only consumer documented in `docs/reference/known-consumers.md`) or a follow-up ADR retiring it.

2. **`zally-core` width.** The PRD names many domain types. If `zally-core` grows beyond ~20 public types, the criteria for splitting it (e.g., into `zally-core-types` and `zally-core-errors`) need a new ADR. Default for v1: one core crate.

3. **`zally-storage` and `zally-chain` feature flags.** Both expose default implementations (`SqliteWalletStorage`, `ZinderChainSource`) that pull heavy dependencies (`rusqlite`, `tonic`). Should the trait-only surface be available without those dependencies via a `no-default-features` build? Default lean: yes; consumers integrating against a non-default chain source or storage backend should not have to compile `rusqlite` or `tonic`. Decision deferred to the first ADR that exercises this constraint.
