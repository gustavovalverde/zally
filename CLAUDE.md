# Claude Code Conventions

Claude Code specifics for Zally. Read [AGENTS.md](AGENTS.md) first; this file only adds Claude-specific guidance.

## Working directories

Zally consumes [librustzcash](https://github.com/zcash/librustzcash) as a versioned dependency; never fork it. ZIP-compliance work cross-references the [Zcash ZIPs](https://github.com/zcash/zips) corpus.

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

For v0.2.3 release work, see the [scoped dependency policy check](CHANGELOG.md#bundled-sqlite-advisory).

## Live node tests

Use the [live-test runbook](docs/runbooks/live-test-setup.md) to select the
network, configure the endpoint, and run the matching tests.

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

- `/init`: fresh checkout only. Already done here; do not re-run.
- `/review`: review a Zally PR.
- `/security-review`: pre-merge security review on the current branch. Mandatory for any change affecting key custody, signing, or transaction construction.
- `/identifier-naming`: apply the naming guard before declaring any new identifier.
- `/codebase-structure`: apply the structure guard before adding any file or directory.

## Tools

Prefer `Edit` for surgical changes; reserve `Write` for new files or full rewrites. Never run `git push`, `gh pr create`, `gh pr review --approve`, `gh pr review --request-changes`, or any destructive git command (`reset --hard`, `clean -f`, `branch -D`, force-push) without explicit confirmation in the same turn.

When investigating across upstream repositories (librustzcash, zips), dispatch the `Explore` subagent rather than reading every file inline. Cite file paths and line numbers in findings.

## When you are done

Mark all your tasks completed before ending the session. If you discovered new follow-up work, file it as a GitHub issue rather than leaving it as a stale task.
