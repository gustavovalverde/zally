# Zally Documentation

This documentation set defines Zally's product scope, library boundaries, and naming conventions for a typed Rust headless-wallet library on top of librustzcash.

## Product

- [PRD-0001: Typed Rust library for headless server-side Zcash wallets](prd-0001-zally-headless-wallet-library.md)

## Architecture

- [Public interfaces](architecture/public-interfaces.md) — the vocabulary spine. Every other doc, ADR, type, and config field defers to the conventions here. Read this before writing any public type.

## ADRs

- [ADR-0001: Workspace crate boundaries](adrs/0001-workspace-crate-boundaries.md)
- [ADR-0002: Founding implementation patterns](adrs/0002-founding-implementation-patterns.md)

## RFCs

- [RFC-0001: Slice 1 — Open a Wallet](rfcs/0001-slice-1-open-wallet.md) — Accepted
- [RFC-0002: Slice 2 — Chain Source and Wallet Sync](rfcs/0002-slice-2-chain-and-sync.md) — Accepted
- [RFC-0003: Slice 3 — Spend (propose, sign, send)](rfcs/0003-slice-3-spend.md) — Accepted
- [RFC-0004: Slice 4 — PCZT Roles](rfcs/0004-slice-4-pczt.md) — Accepted
- [RFC-0005: Slice 5 — Observability, Cookbook, and v1 Acceptance](rfcs/0005-slice-5-observability-and-cookbook.md) — Accepted

RFCs land in `rfcs/` ahead of substantial multi-PR architectural shifts; accepted RFCs spawn one or more ADRs.

## Reference

Living external constraints, distilled audit findings, and prior-art summaries. Refreshed as the upstream world changes.

- [v1 follow-up inventory](reference/v1-follow-up.md) — every deferred surface that lands after v1.0: live ZinderChainSource, live block scanning, live send, live PCZT signing, LightwalletdChainSource, retry/circuit-breaker harness, transparent gap-limit.

Planned but not yet written:

- `reference/librustzcash-version-policy.md` — how Zally tracks `zcash_client_backend` / `zcash_client_sqlite` minor bumps.
- `reference/zip-compliance.md` — per-ZIP status table tracking which ZIPs Zally implements and at what coverage.
- `reference/error-vocabulary.md` — full enum of typed errors across all crates with retry posture per variant.

## Runbooks

Operational procedures for running Zally against the workspace and external systems.

- [Bootstrap an operator wallet](runbooks/bootstrap-operator-wallet.md) — sealing choice, `Wallet::create` flow, mnemonic capture, operator checklist.
- [Live-test setup](runbooks/live-test-setup.md) — regtest / testnet / mainnet wiring for `ZALLY_TEST_LIVE=1` tests; profiles and gating.
- [Zinder live bring-up](runbooks/zinder-live-bringup.md) — start `zinder-ingest` + `zinder-query` against a running Zebra and connect Zally's `ZinderChainSource`. Verified for regtest (port 9101) and testnet (port 9203).
- [Sweep with PCZT](runbooks/sweep-with-pczt.md) — operator-facing flow that walks the Creator → Signer → Combiner → Extractor role chain; remains the contract once the v1 follow-up wires deep proposal construction.

Planned but not yet written:

- `runbooks/key-rotation.md` — migrate from one seed to another with funds in flight.

## Document lifecycles

Each tree under `docs/` has its own retire-on-ship rule.

- **PRD**: edited in place for clarifications. Substantive scope changes spawn a new PRD with an incremented number.
- **Architecture**: the durable spine. Explains why each contract exists, what its invariants are, and where its boundary lives. Edited in place when the contract changes. Updated in the same PR as the code that alters the contract. References other architecture docs and at most one ADR per topic.
- **ADRs**: numbered, present-tense records of accepted decisions. Edited in place for clarifications; substantive design changes get a new ADR with a contiguous number that supersedes the old.
- **RFCs**: pre-decision boundary contracts. Accepted RFCs become the architectural spine; they reference architecture docs, not each other.
- **Reference**: living external constraints. Anti-pattern catalogues, integration requirements, upstream surface audits. Refreshed when upstream changes invalidate the captured state.
- **Runbooks**: operational procedures with explicit prereqs, command lines, and expected outcomes. Edited in place as procedures evolve; reference architecture docs and ADRs but do not describe architectural intent.
