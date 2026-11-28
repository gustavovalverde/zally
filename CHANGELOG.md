# Changelog

## 0.3.0

Zally uses the Zinder 0.29.0 native WalletQuery SDK. Hosts that construct an
`EndpointBackedIndex` for `ZinderChainSource::from_chain_index` or
`ZinderSubmitter::from_chain_index` must use `zinder-client = "=0.29.0"`.
Align `zinder-core` to 0.29.0 wherever you exchange its types with Zally.
To connect by URL, use the same endpoint and network with
`ZinderChainSource::connect_remote`. Use Rust 1.100 or later and a Zinder
endpoint that supports native contract revision 8 or later. Move all Zally
Git dependencies to `tag = "v0.3.0"` together.

After restarting on a quiet chain, the sync driver can confirm an already
caught-up wallet by checking its stored tip against the chain. Faucet and
other hosts that gate payouts on a fresh sync observation can resume at the
verified tip without waiting for a new block.

### SQLite security

The bundled SQLite remains affected by RUSTSEC-2026-0459. A wallet process can
disclose memory when it opens a database and write-ahead log that an
untrusted party could write. Keep both files in a directory writable only
by the wallet operator and avoid opening untrusted copies.
[RustSec advisory](https://rustsec.org/advisories/RUSTSEC-2026-0459.html)

The owner approved a one-release exception for Zally v0.3.0 and the locked
`libsqlite3-sys` 0.35.0. Run `python3 scripts/check-dependencies.py` from a
full checkout to report the standard result and recheck all policies with
that advisory excepted. The exception expires when the checkout contains
the v0.3.0 tag or a workspace crate changes version.

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

The v0.2.3 release used a one-release policy exception for this advisory
and the locked `libsqlite3-sys` 0.35.0. The exception expired when v0.2.3
was tagged. A compatible published release of the wallet SQLite cohort is
needed to retire the exception script.
[Upstream coordination](https://github.com/zcash/librustzcash/issues/3276)

## Earlier releases

See [GitHub Releases](https://github.com/gustavovalverde/zally/releases) for
changes and upgrade notes for v0.2.2 and earlier.
