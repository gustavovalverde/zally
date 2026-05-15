# Zally

Typed Rust library for headless server-side Zcash wallets.

Zally is the library-shaped sibling to [Zallet](https://github.com/zcash/wallet). Both consume librustzcash; both produce ZIP-compliant Zcash transactions. They differ in product shape: Zallet is a JSON-RPC wallet daemon for operators who want a separate wallet process; Zally is an in-process Rust dependency for operators who want a library they link into their server. Faucets, exchanges, payment processors, custody backends, and mining pools are the canonical Zally consumers.

## Why Zally

The Zcash ecosystem has all the wallet primitives (librustzcash, `zcash_client_backend`, `zcash_client_sqlite`, PCZT) but no product shape that fits a server operator. Zashi and Zodl are mobile-shaped. Zingolib is single-user wallet-data-file shaped. Zallet is daemon-shaped and its maintainers state explicitly that the Rust API is not a stable contract. `zcash-devtool` is one-shot CLI commands and warns it is not for production. Every team building a server-side Zcash integration has been reinventing wallet logic on top of `zcash_client_backend` or wrapping Zallet's RPC and paying the daemon-shaped tax on every operation.

Zally fills the missing rung: typed, in-process, non-interactive, multi-receiver, PCZT-first, encrypted-at-rest, observable. One operator identity per process. No exposed RPC. No long-lived daemon semantics built in.

## Approach

- **librustzcash as the foundation, not a fork.** Bug fixes and protocol changes flow upstream. Zally absorbs the API churn behind its own surface so operators see stable contracts across librustzcash releases.
- **Pluggable boundaries.** Chain source, transaction submission, and storage are each behind a trait. Operators swap implementations without forking Zally.
- **Operator-grade key custody by default.** Seeds encrypted at rest. Spending keys held in memory only during signing. Optional PCZT export for HSMs, FROST quorums, and air-gapped signers.
- **ZIP compliance enforced at the API boundary.** Memo on a transparent recipient is a typed error. Shielded input paying a TEX address is a typed error. Coinbase note below maturity is a typed error. Operators cannot accidentally violate protocol invariants.
- **Anti-entropy by construction.** Strict workspace lints (`unsafe = "forbid"`, `unwrap_used = "deny"`, full clippy `pedantic`). Network-tagged types throughout. Typed errors with documented retry posture. No filler nouns, no generic suffixes, no temporal adjectives in identifiers.

## Architecture at a glance

```
                                  ┌──────────────────────────┐
                                  │   librustzcash crates    │
                                  │ (crypto, wallet state,   │
                                  │  proposal builder, PCZT) │
                                  └────────────┬─────────────┘
                                               │
                                               │ versioned dependency
                                               ▼
                ┌──────────────────────────────────────────────────────────┐
                │                          Zally                           │
                │                                                          │
                │  zally-wallet  ◄──── high-level operator API             │
                │       │                                                  │
                │       ├──► zally-chain   (ChainSource + Submitter)       │
                │       ├──► zally-storage (WalletStorage trait + SQLite)  │
                │       ├──► zally-keys    (seed sealing, derivation)      │
                │       ├──► zally-pczt    (PCZT roles)                    │
                │       └──► zally-core    (Network, Zatoshis, errors)     │
                │                                                          │
                │  zally-testkit ──── fixtures + regtest helpers           │
                │                                                          │
                └──────────────────────────────────────────────────────────┘
                                               │
                                               │ pluggable ChainSource + Submitter
                                               ▼
                                  ┌──────────────────────────┐
                                  │  Operator-chosen backend │
                                  │  (a zinder-backed impl   │
                                  │   ships behind a feature │
                                  │   flag; bring your own.) │
                                  └──────────────────────────┘
```

## Workspace

Crate boundaries are recorded in [ADR-0001](docs/adrs/0001-workspace-crate-boundaries.md).

- `zally-core`: domain types (`Network`, `Zatoshis`, `BlockHeight`, `AccountId`, `ReceiverPurpose`, error enums). Zero domain-foreign dependencies.
- `zally-keys`: seed lifecycle, encryption-at-rest (`SeedSealing` trait), USK/UFVK/UIVK derivation, zeroization discipline.
- `zally-storage`: `WalletStorage` trait wrapping librustzcash's `WalletRead`/`WalletWrite`; default `SqliteWalletStorage` over `zcash_client_sqlite`.
- `zally-chain`: `ChainSource` and `Submitter` traits. A `ZinderChainSource` plus `ZinderSubmitter` implementation ships behind the `zinder` cargo feature; operators with a different chain plane provide their own implementation of the traits.
- `zally-pczt`: PCZT roles (`Creator`, `Prover`, `Signer`, `Combiner`, `Extractor`) for HSM and multi-party signing.
- `zally-wallet`: high-level operator API. Includes the scan-loop module (orchestrating `ChainSource` plus `WalletStorage`).
- `zally-testkit`: fixtures, mock chain sources, in-memory storage, regtest helpers. Behind a feature flag so it never lands in operator binaries.

## Validation gate

Every change must pass:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --profile=ci
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps
cargo deny check
cargo machete
```

Live-node tests (T3) run on demand:

```sh
ZALLY_TEST_LIVE=1 ZALLY_NETWORK=regtest cargo nextest run --profile=ci-live --run-ignored=all
```

## Documentation

Start here:

- [Public interfaces](docs/architecture/public-interfaces.md): the vocabulary spine. Naming rules, error vocabulary, type conventions, config rules, capability surface, ZIP coverage. Read before writing any public type.
- [ADR-0001](docs/adrs/0001-workspace-crate-boundaries.md): workspace crate boundaries.
- [ADR-0002](docs/adrs/0002-implementation-patterns.md): implementation patterns shared by every crate.
- [Documentation index](docs/README.md): full index with lifecycle rules.

## License

MIT. See [LICENSE](LICENSE).

## Contributing

Read [AGENTS.md](AGENTS.md) for the universal conventions. Open an issue before non-trivial work; agree on scope, then implement.

Substantive changes (new crates, new traits, ZIP-compliance shifts) require an ADR. Trivial changes (typo fixes, dependency bumps within range, docs polish) do not.
