# Claude Code Conventions

Claude Code specifics for Zally. Read [AGENTS.md](AGENTS.md) first; this file only adds Claude-specific guidance.

## Working directories

Zally develops alongside three sibling repositories. Add them to your session if you need to read them:

- `/Users/gustavovalverde/dev/zfnd/librustzcash` — cryptographic and wallet-state foundation. Zally is a versioned consumer; never fork.
- `/Users/gustavovalverde/dev/zfnd/zinder` — chain-read plane; default `ChainSource` implementation targets it.
- `/Users/gustavovalverde/dev/zfnd/fauzec` — primary first consumer; integration testbed for Zally's public API.
- `/Users/gustavovalverde/dev/zfnd/zips` — Zcash Improvement Proposals corpus; Zally's compliance ceiling.

## Reading the spine before edits

Before changing anything in `crates/`, `docs/architecture/`, or `docs/adrs/`, read [docs/architecture/public-interfaces.md](docs/architecture/public-interfaces.md). The spine locks naming, vocabulary, error categories, and config conventions. Drift here costs more to revert than skipping the read costs to commit.

Before adding a new crate, read [docs/adrs/0001-workspace-crate-boundaries.md](docs/adrs/0001-workspace-crate-boundaries.md). New crates require an ADR amendment or a new ADR.

## Default validation gate

Run before every push:

```sh
cargo fmt --all --check && \
cargo check --workspace --all-targets --all-features && \
cargo clippy --workspace --all-targets --all-features -- -D warnings && \
cargo nextest run --profile=ci && \
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps && \
cargo deny check && \
cargo machete
```

The PR validation gate matches this command.

## Live node tests

T3 live tests against z3 regtest:

```sh
ZALLY_TEST_LIVE=1 \
  ZALLY_NETWORK=regtest \
  ZALLY_NODE__JSON_RPC_ADDR=http://127.0.0.1:18232 \
  ZALLY_NODE__COOKIE_VALUE=__cookie__:<value> \
  cargo nextest run --profile=ci-live --run-ignored=all
```

Public testnet:

```sh
ZALLY_TEST_LIVE=1 \
  ZALLY_NETWORK=testnet \
  ZALLY_NODE__JSON_RPC_ADDR=https://testnet-zebra.example/ \
  cargo nextest run --profile=ci-live --run-ignored=all
```

Mainnet requires `ZALLY_TEST_ALLOW_MAINNET=1` in addition. Never run against an operator-owned mainnet wallet without explicit confirmation in the same turn.

## Coding constraints

- No `unsafe`. No `unwrap`, `expect`, `panic`, `todo`, `unimplemented`. The lint baseline enforces.
- No `Box<dyn Error>`. No `anyhow` in library crates. `thiserror` v2 typed enums per boundary.
- No filler nouns in any identifier (`data`, `info`, `value`, `item`, `result`, `tmp`, `payload`, `obj`, `foo`, `bar`).
- No generic type suffixes (`*Manager`, `*Processor`, `*Helper`, `*Util`, `*Service`, `*Data`, `*Info`).
- No temporal or implementation drift in names (`new_x`, `x2`, `legacy_x`, `redis_x`, `sqlite_x`).
- No em dashes anywhere.
- No code comments unless the *why* is non-obvious. The code is the *what*.
- No `Co-Authored-By:` trailer unless explicitly required.
- Network-tagged types throughout. Functions that take an address without a network are review-blocking.

## Skills that match common Zally work

- `/init` — fresh checkout only. Already done here; do not re-run.
- `/review` — review a Zally PR.
- `/security-review` — pre-merge security review on the current branch. Mandatory for any change affecting key custody, signing, or transaction construction.
- `/identifier-naming` — apply the naming guard before declaring any new identifier.
- `/codebase-structure` — apply the structure guard before adding any file or directory.

## Tools

Prefer `Edit` for surgical changes; reserve `Write` for new files or full rewrites. Never run `git push`, `gh pr create`, `gh pr review --approve`, `gh pr review --request-changes`, or any destructive git command (`reset --hard`, `clean -f`, `branch -D`, force-push) without explicit confirmation in the same turn.

When investigating across sibling repositories (librustzcash, zinder, fauzec, zips), dispatch the `Explore` subagent rather than reading every file inline. Cite file paths and line numbers in findings.

## When you are done

Mark all your tasks completed before ending the session. If you discovered new follow-up work, file it as a GitHub issue rather than leaving it as a stale task.
