# Zally Documentation

This documentation set defines Zally's product scope, library boundaries, and naming conventions for a typed Rust headless-wallet library on top of librustzcash.

## Architecture

- [Public interfaces](architecture/public-interfaces.md): the vocabulary spine. Every other doc, ADR, type, and config field defers to the conventions here. Read this before writing any public type.

## ADRs

- [ADR-0001: Workspace crate boundaries](adrs/0001-workspace-crate-boundaries.md)
- [ADR-0002: Implementation patterns](adrs/0002-implementation-patterns.md)

## Runbooks

Operational procedures for running Zally against the workspace and external systems.

- [Bootstrap an operator wallet](runbooks/bootstrap-operator-wallet.md): sealing choice, `Wallet::create` flow, mnemonic capture, operator checklist.
- [Live-test setup](runbooks/live-test-setup.md): regtest, testnet, and mainnet wiring for `ZALLY_TEST_LIVE=1` tests, plus bringing up the zinder-backed chain source.
- [Sweep with PCZT](runbooks/sweep-with-pczt.md): operator-facing flow that walks the Creator, Signer, Combiner, Extractor role chain.

## Document lifecycles

Each tree under `docs/` has its own retire-on-ship rule.

- **Architecture**: the durable spine. Explains why each contract exists, what its invariants are, and where its boundary lives. Edited in place when the contract changes. Updated in the same PR as the code that alters the contract. References other architecture docs and at most one ADR per topic.
- **ADRs**: numbered, present-tense records of accepted decisions. Edited in place for clarifications; substantive design changes get a new ADR with a contiguous number that supersedes the old.
- **Runbooks**: operational procedures with explicit prereqs, command lines, and expected outcomes. Edited in place as procedures evolve; reference architecture docs and ADRs but do not describe architectural intent.
