# Operational Surfaces

Zally is library-shaped. It does not own a process, RPC listener, config file, signal handler, or package format. It does own wallet-local operational contracts that every embedding application would otherwise have to reinvent.

## Boundaries

| Crate | Operational responsibility |
|-------|----------------------------|
| `zally-chain` | Source-neutral chain reads, chain-event envelopes, and transaction submission. |
| `zally-storage` | Durable wallet state and scanner persistence. |
| `zally-wallet` | Wallet lifecycle, status, sync driver, proposal, signing, submission orchestration, and wallet events. |
| `zally-testkit` | In-process mocks, live-test gates, and live-test environment conventions. |

Zinder remains a `ChainSource` and `Submitter` implementation. Zally public wallet APIs must not expose Zinder cursor types, Zinder readiness states, or Zinder service names.

## Status

`Wallet::status_snapshot()` is the readiness surface. It returns `WalletStatus`, including:

- `SyncStatus`: scan lifecycle derived from persisted wallet progress.
- `scanned_height`: the wallet's durable scan height.
- `chain_tip_height`: the most recent tip observed by sync.
- `lag_blocks`: known scan lag when the tip has not regressed.
- `circuit_breaker`: current outbound IO breaker state.

`Wallet::metrics_snapshot()` is a metrics adapter source. It must be derivable from `WalletStatus` and must not invent a second truth for scan progress.

## Sync Driver

`SyncDriver` is a caller-owned background loop. The host process owns the Tokio runtime and shutdown policy; the driver owns only wallet catch-up.

The driver:

- listens to `ChainSource::chain_event_envelopes`;
- resumes from the last in-process `ChainEventCursor` after a stream reconnect;
- falls back to polling with `poll_interval_ms`;
- calls `Wallet::sync` repeatedly until the observed tip is reached or `max_sync_iterations_per_wake_count` is exhausted;
- publishes `SyncSnapshot` values through `SyncHandle::observe_status`;
- stops through `SyncHandle::close`.

The driver does not:

- expose RPC;
- start Zinder or a node;
- read TOML;
- own process signals;
- hide non-retryable wallet errors.

## Bootstrap

`Wallet::open_or_create_account(...) -> (Wallet, AccountId)` is the deployment bootstrap path.
It opens the account for the sealed seed when storage is warm. If the persistent volume is fresh,
it creates that account from the sealed seed at the configured birthday height.

The caller does not need an `Opened` versus `Restored` branch: the useful postcondition is the
same in both cases, one wallet handle and one account id bound to the sealed seed.

## Live Proof

T3 tests use real local infrastructure and are ignored by default. The funded Zinder test requires:

- `ZALLY_TEST_LIVE=1`
- `ZALLY_NETWORK=regtest`
- `ZINDER_ENDPOINT`

Optional overrides:

- `ZALLY_TEST_NODE_JSON_RPC_ADDR` (defaults to `http://127.0.0.1:39232/`)
- `ZALLY_TEST_NODE_RPC_USER` and `ZALLY_TEST_NODE_RPC_PASSWORD` when the node requires basic auth
- `ZALLY_TEST_SHIELDING_THRESHOLD_ZAT` (defaults to `1000000`)
- `ZALLY_TEST_SEND_ZAT` (defaults to `10000`)

The funded proof does not require Zallet or a separate funder wallet. It derives the
regtest activation table from the running node, funds a Zally transparent receiver with the
testkit transparent signer, shields through `Wallet::shield_transparent_funds`, sends through
`Wallet::send_payment`, and completes the PCZT path with
`Wallet::propose_pczt`, `Wallet::prove_pczt`, `Wallet::sign_pczt`, and
`Wallet::extract_and_submit_pczt`.
