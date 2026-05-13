# Repository Guidelines

These conventions apply to every contributor, human or AI agent, working on Zally.

## Project structure and module organisation

Zally is a Rust 2024 workspace. Crates live under `crates/` (added as code lands; see [ADR-0001](docs/adrs/0001-workspace-crate-boundaries.md) for the planned boundary set). There is no `services/` directory; Zally is library-shaped. There is no `utils/`, `helpers/`, `shared/`, or `common/` directory at any level — code with no bounded context has nowhere to live.

Documentation is under `docs/` with this layout:

- `docs/prd-NNNN-<slug>.md` — product requirements (numbered, present tense)
- `docs/architecture/public-interfaces.md` — vocabulary spine (mandatory; first thing to read)
- `docs/architecture/<topic>.md` — boundary contracts (living, edited in place)
- `docs/adrs/NNNN-<slug>.md` — accepted decisions (numbered, present tense)
- `docs/rfcs/NNNN-<slug>.md` — pre-decision contracts (accepted RFCs become the spine)
- `docs/reference/<topic>.md` — living external constraints, audit findings, prior-art summaries
- `docs/runbooks/<task>.md` — operational procedures with explicit commands

## Build, test, and development commands

The default validation gate (every PR; matches [README §Validation gate](README.md#validation-gate)):

- `cargo fmt --all --check` — verify formatting.
- `cargo check --workspace --all-targets --all-features` — type-check every crate and test target.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — strict lint gate.
- `cargo nextest run --profile=ci` — T0 unit and T1 integration tests.
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps` — validate rustdoc.
- `cargo deny check` — dependency policy.
- `cargo machete` — unused dependencies.

Live (T3) tests run on demand:

- `ZALLY_TEST_LIVE=1 ZALLY_NETWORK=regtest cargo nextest run --profile=ci-live --run-ignored=all`

Mainnet live tests are gated behind `ZALLY_TEST_ALLOW_MAINNET=1` in addition to `ZALLY_TEST_LIVE=1`. Production binaries strip every key starting with `ZALLY_TEST_` from env reads.

## Coding style and naming conventions

Workspace-managed Rust 2024 settings per `rustfmt.toml`. Lint baseline in `Cargo.toml` denies warnings, `unsafe`, `unwrap`, `expect`, `panic`, `todo`, debug prints, and unreachable public API.

Identifier discipline (the [public interfaces spine](docs/architecture/public-interfaces.md) is the canonical reference):

- **Forbidden generic roots**, anywhere in any identifier: `utils`, `helpers`, `common`, `shared`, `manager`, `handler`, `processor`, `data`, `info`, `item`, `result`, `stuff`, `thing`, `tmp`, `value`, `payload`. As suffixes: `service`, `server`, `api`.
- **Required suffixes** on numeric and lifecycle identifiers:
  - Duration: `_ms`, `_seconds`, `_minutes`, `_hours`, `_blocks`, `_height`. Never bare `timeout`, `delay`, `interval`.
  - Money: `_zat` for integer zatoshis, `_zec` for decimal-string ZEC. Never bare `amount`.
  - Booleans: `is_*`, `has_*`, `can_*`. Never bare `enabled`, never negated names like `is_not_ready`.
- **Network-tagged types throughout.** Every public type that names an address, key, balance, or transaction carries a `Network` value or is generic over a `NetworkSpec` parameter. A function that takes an address but not a network is a review-blocking smell.
- **Verbs from the project vocabulary** (`get`, `find`, `compute`, `derive`, `propose`, `sign`, `submit`, `observe`, `sync`, `seal`, `unseal`, `export`). Generic verbs (`handle`, `process`, `manage`, `do`, `perform`, `execute`) are forbidden for domain operations.
- **No type suffixes that signal missing abstraction** (`*Manager`, `*Processor`, `*Helper`, `*Util`, `*Service`, `*Data`, `*Info`). Kept: `*Error` (extends `Error`), `*Trait` only when disambiguation is needed.
- **No temporal or implementation drift in names.** `new_x`, `x2`, `legacy_x`, `x_old`, `x_final`, `x_real`, `redis_x`, `sqlite_x` are all banned. The name of a thing must survive a change of its implementation.

Test naming: T3 live test function names are plain `snake_case_describing_behavior`. Do not include `live`, `regtest`, `testnet`, or `mainnet` in the function name; directory and runtime parameterisation carry that.

## Error vocabulary

`thiserror` v2 throughout. Each public boundary returns a typed enum; no `Box<dyn Error>`, no `anyhow`, no `Other(String)` catch-alls. Each error variant has a documented retry posture (`retryable`, `not_retryable`, `requires_operator`) in its rustdoc. The vocabulary is recorded in `docs/reference/error-vocabulary.md` (added as the first crate's errors land); a new error variant requires an entry there before merging.

## Testing guidelines

Tier organisation matches Zinder's ADR-0006:

| Tier | Location | Nextest profile |
|------|----------|----------------|
| T0 unit | `#[cfg(test)] mod tests` inside `src/` | default |
| T1 integration | `tests/integration/` per crate | default |
| T2 perf | `tests/perf/` per crate | `ci-perf` profile |
| T3 live | `tests/live/` per crate | `ci-live` profile |

Each crate's `tests/acceptance.rs` aggregates the tier submodules via `mod integration;` (and `mod live;` etc.).

T3 tests are double-gated by `#[ignore = LIVE_TEST_IGNORE_REASON]` and a runtime `require_live()` call from `zally-testkit`. Mainnet is rejected by default; opt in with `ZALLY_TEST_ALLOW_MAINNET=1`.

## Commit and pull-request guidelines

Concise imperative commits with an optional scope: `wallet: refuse memo on transparent recipient`. No `Co-Authored-By:` trailer on AI-assisted commits unless the model is explicitly named. No "Generated with Claude Code" footer. No em dashes anywhere (code, docs, commits, PR descriptions); use colons, semicolons, parentheses, or restructure.

Pull requests:

- Lead with user-facing impact, not implementation.
- Summarise behaviour changes; list validation commands run; link related docs or ADR updates.
- Call out any deferred production gap.
- Substantive changes (new crates, new trait surface, ZIP-compliance shifts) cite the ADR they implement.

## Security and configuration

- Never print secrets or raw key material. `--print-config` (when a binary is added) must show `[REDACTED]` for every secret field.
- Seeds at rest are sealed by default (`SeedSealing` trait, default age-encrypted file). Plain-text seed storage requires opt-in via `unsafe_plaintext_seed` and emits a WARN-level log on every open.
- Network mismatches must fail closed at construction time, not at signing time.
- Mainnet operations require an explicit `--mainnet` config or builder call; defaults are testnet-first.

## Sibling repositories

Zally lives in an ecosystem with three sibling projects:

- [zinder](https://github.com/gustavovalverde/zinder) — service-oriented Zcash indexer; Zally's default chain-read plane.
- [fauzec](https://github.com/gustavovalverde/fauzec) — Zcash testnet faucet; primary first consumer of Zally.
- [zallet](https://github.com/zcash/wallet) — Zcash wallet daemon; the sibling product Zally is the library shape of.

Conventions inherit from Zinder where applicable; deviations are explicit and justified in an ADR.
