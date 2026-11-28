# Changelog

## Unreleased

Zally uses the Zinder 0.28.0 native WalletQuery SDK. Hosts that construct an
`EndpointBackedIndex` for `ZinderChainSource::from_chain_index` or
`ZinderSubmitter::from_chain_index` must use `zinder-client = "=0.28.0"`.
Align `zinder-core` to 0.28.0 wherever you exchange its types with Zally.
To connect by URL, use the same endpoint and network with
`ZinderChainSource::connect_remote`. The minimum server contract revision is 8.

## 0.2.3

Zally now uses the Zinder 0.23.0 native WalletQuery SDK. Use Rust 1.100 or later
and a Zinder endpoint that supports native contract revision 8 or later.
The wallet database schema is unchanged. Move every Zally Git dependency to
`tag = "v0.2.3"` together.

If your application pins librustzcash crates directly, align
`zcash_primitives` with 0.31.1 and `zcash_transparent` with 0.11.1 before
updating the tag.

Hosts using `ZinderChainSource::connect_remote` can keep their existing
endpoint and network configuration. If your host passes an existing
`EndpointBackedIndex` to `ZinderChainSource::from_chain_index` or
`ZinderSubmitter::from_chain_index`, build the handle with
`zinder-client = "=0.23.0"`. Rust treats a handle from an earlier SDK as a
different type. Align `zinder-core` to 0.23.0 wherever you exchange its types
with Zally.

The dependency refresh also updates rustix to 1.1.6, which fixes an
uninitialized-memory issue in extended attribute reads.
[RustSec advisory](https://rustsec.org/advisories/RUSTSEC-2026-0486.html)

The [Regtest lifecycle procedure](docs/runbooks/live-test-setup.md#native-wallet-lifecycle-conformance)
covers wallet creation, restore, persisted state across process restarts, and
the same shielding transaction through submission, mining, and scanning.
The [public Testnet probe](docs/runbooks/live-test-setup.md#public-testnet)
checks connectivity, wallet synchronization, and settled tree state.

### Bundled SQLite advisory

The wallet still bundles `libsqlite3-sys` 0.35.0 through the
`zcash_client_sqlite` 0.23.0 dependency cohort. SQLite can disclose local
process memory when opening a database and its WAL that an untrusted party
could write. Keep wallet databases and WAL files in directories writable only
by the wallet operator and avoid opening untrusted copies. The bundled copy
remains affected. [RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459)

For the v0.2.3 release work, run `python3 scripts/check-dependencies.py`.
It reports the original policy result, then rechecks all dependency policies
with only this advisory excepted for the locked `libsqlite3-sys` 0.35.0.
The exception is inactive once the full checkout contains the v0.2.3 tag or
any workspace crate changes version. A compatible published release of the
wallet SQLite cohort is needed to remove the exception code.
[Upstream coordination](https://github.com/zcash/librustzcash/issues/3276)

## Earlier releases

See [GitHub Releases](https://github.com/gustavovalverde/zally/releases) for
changes and upgrade notes for v0.2.2 and earlier.
