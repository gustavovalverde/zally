# Zally

Typed Rust library for headless server-side Zcash wallets.

Zally is an in-process Rust dependency for server applications that need wallet capability without running a wallet daemon. It uses librustzcash for Zcash protocol machinery and exposes Zally-owned types for wallet lifecycle, chain reads, transaction submission, seed sealing, PCZT workflows, and observable sync state.

## Capabilities

- Typed wallet API for create, open, sync, status, address derivation, spend, shield, and PCZT flows.
- Network-tagged public types for addresses, balances, accounts, transactions, and chain heights.
- Seed sealing through `SeedSealing`, with age-encrypted file sealing as the default implementation.
- Pluggable `ChainSource`, `Submitter`, and `WalletStorage` boundaries.
- Structured wallet events, sync snapshots, metrics snapshots, retry posture, and circuit-breaker state.
- ZIP-302 memo guard, ZIP-316 Unified Addresses, ZIP-317 conventional fees, ZIP-320 TEX recognition, and ZIP-321 payment URI parsing.

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
- [Documentation index](docs/README.md): architecture, reference, and runbook index.

## License

MIT. See [LICENSE](LICENSE).

## Contributing

Read [AGENTS.md](AGENTS.md) for repository conventions.
