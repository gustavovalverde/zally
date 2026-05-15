# Zally Documentation

This documentation set defines Zally's product scope, library boundaries, and naming conventions for a typed Rust headless-wallet library on top of librustzcash.

## Architecture

- [Public interfaces](architecture/public-interfaces.md): the vocabulary spine. Every other doc, ADR, type, and config field defers to the conventions here. Read this before writing any public type.
- [Operational surfaces](architecture/operational-surfaces.md): wallet status, storage execution, sync driver, bootstrap outcome, and live-proof contracts.

## ADRs

- [ADR-0001: Workspace crate boundaries](adrs/0001-workspace-crate-boundaries.md)

## Reference

- [Error vocabulary](reference/error-vocabulary.md): public error variants and retry posture.

## Runbooks

Operational procedures for running Zally against the workspace and external systems.

- [Bootstrap an operator wallet](runbooks/bootstrap-operator-wallet.md): sealing choice, `Wallet::create` flow, mnemonic capture, operator checklist.
- [Live-test setup](runbooks/live-test-setup.md): regtest, testnet, and mainnet wiring for `ZALLY_TEST_LIVE=1` tests, plus bringing up the zinder-backed chain source.
- [Sweep with PCZT](runbooks/sweep-with-pczt.md): operator-facing flow that walks the Creator, Prover, Signer, Combiner, Extractor role chain.
