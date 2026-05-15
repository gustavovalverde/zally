# ADR-0003: Zally Owns Embeddable Wallet Operations

## Status

Accepted.

## Context

Zally is the library-shaped peer to Zallet. Zallet owns a daemon process, RPC compatibility, config files, task supervision, and distribution artifacts. Zally must not copy that product shape, but it still needs to prevent every embedding application from writing its own wallet sync loop, readiness model, and bootstrap interpretation.

Before this ADR, Zally had the correct primitives:

- `ChainSource` and `Submitter` abstracted the chain data plane.
- `Wallet::sync` performed a one-shot scan.
- `WalletEvent` streamed wallet observations.
- `Wallet::open_or_create_account` covered sealed-seed recovery.

The missing contract was operational: callers had no source-neutral long-lived sync driver, no durable status vocabulary, and no cursor-bound event boundary.

## Decision

Zally owns embeddable wallet operations inside `zally-wallet`.

This includes:

- `WalletStatus` and `SyncStatus` for readiness.
- `SyncDriver`, `SyncDriverOptions`, `SyncHandle`, and `SyncSnapshot` for caller-owned continuous sync.
- `Wallet::open_or_create_account(...) -> (Wallet, AccountId)` for sealed-seed bootstrap on fresh or warm storage.
- Source-neutral `ChainEventCursor` and `ChainEventEnvelope` in `zally-chain`.
- T3 funded live tests that prove the real Zinder-backed read and submit path.

Zally does not add:

- `zally-runtime`;
- `zally-server`;
- a `services/` tree;
- a JSON-RPC listener;
- process signal handling;
- a Zally-owned TOML config.

## Consequences

Embedding applications get a stable operational contract without adopting a daemon. They still own process lifecycle, application config, secret provisioning, and observability export.

Agents get predictable names and source-neutral boundaries:

- `SyncDriver`, not `SyncManager` or `SyncService`.
- `SyncStatus`, not Zinder-specific status.
- `ChainEventCursor`, not Zinder's cursor type.
- `open_or_create_account`, not separate open and restore branches in embedding applications.

Zinder remains a chain data plane. It may provide chain events, cursors, compact blocks, tree states, and broadcast, but it does not own wallet scanning, key custody, wallet readiness, or local wallet state.
