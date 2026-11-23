# Zally

Typed Rust library for headless server-side Zcash wallets.

Zally is an in-process Rust dependency for server applications that need wallet capability without running a wallet daemon. It uses librustzcash for Zcash protocol machinery and exposes Zally-owned types for wallet lifecycle, chain reads, transaction submission, seed sealing, PCZT workflows, and observable sync state.

## Capabilities

- Typed wallet API for create, open, sync, status, address derivation, spend, shield, PCZT, and payment-disclosure flows.
- Network-tagged public types for addresses, balances, accounts, transactions, and chain heights.
- Seed sealing through `SeedSealing`, with age-encrypted file sealing as the default implementation.
- Pluggable `ChainSource`, `Submitter`, and `WalletStorage` boundaries.
- Structured wallet events, sync snapshots, metrics snapshots, retry posture, and circuit-breaker state.
- ZIP-311 Draft1 Sapling payment disclosures plus an explicitly nonstandard Zally Ironwood profile, ZIP-302 memo guards, ZIP-316 Unified Addresses, ZIP-317 conventional fees, ZIP-320 TEX recognition, and ZIP-321 payment URI parsing.

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

The [v0.2.3 release notes](CHANGELOG.md#023) describe the scoped SQLite
exception and the policy command for that release.

## Integrate Zally

Use the same [Git release tag](https://github.com/gustavovalverde/zally/releases)
for each Zally crate in your application's `Cargo.toml`. Enable `zinder` on
`zally-wallet` for the native Zinder wallet transport. The [live Testnet
probe](docs/runbooks/live-test-setup.md#public-testnet) syncs an unfunded wallet
and reads a settled tree state. Its [source](crates/zally-wallet/examples/live-zinder-probe/main.rs)
shows how to open the chain source, use age sealing and SQLite, and create and
sync a wallet.

## Documentation

Start here:

- [Public interfaces](docs/architecture/public-interfaces.md): the vocabulary spine. Naming rules, error vocabulary, type conventions, config rules, capability surface, ZIP coverage. Read before writing any public type.
- [ADR-0001](docs/adrs/0001-workspace-crate-boundaries.md): workspace crate boundaries.
- [Documentation index](docs/README.md): architecture, reference, and runbook index.

## License

MIT. See [LICENSE](LICENSE).

## Contributing

Install Rust with rustup, then clone the repository. Cargo selects the version
in `rust-toolchain.toml`. Install cargo-nextest, cargo-deny, and cargo-machete
to run the [validation gate](#validation-gate). Include `--all-features` in
the nextest command to exercise the optional Zinder backend. The release policy
script and external consumer build use Python 3.11 or later.

For a dependency update, change the manifest and lockfile together. The
[compatibility workflow](.github/workflows/compatibility.yml) checks the
dependency policy, compares public Rust types against the base revision, and
compiles Zinder's wallet consumer against your checkout. The
[dependency policy ADR](docs/adrs/0006-librustzcash-ironwood-version-floor.md)
explains which Zcash crates move together.

See [AGENTS.md](AGENTS.md) for pull request conventions and the
[changelog](CHANGELOG.md) for changes and upgrade instructions by tag.

### Release

Update all workspace crate versions, the lockfile, and the changelog together.
Once the release PR is reviewed and the required checks pass on the merged
commit, create a signed annotated tag at that commit. Publish the tag with
its changelog entry in GitHub Releases and verify that an external Cargo
project can build against the published tag.
