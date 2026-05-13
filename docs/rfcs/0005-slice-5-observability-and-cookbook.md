# RFC 0005: Slice 5 — Observability, Cookbook, and v1 Acceptance

| Field | Value |
|---|---|
| Status | Accepted |
| Product | Zally |
| Slice | 5 |
| Related | [PRD-0001](../prd-0001-zally-headless-wallet-library.md), [RFC-0001](0001-slice-1-open-wallet.md), [RFC-0002](0002-slice-2-chain-and-sync.md), [RFC-0003](0003-slice-3-spend.md), [RFC-0004](0004-slice-4-pczt.md) |
| Created | 2026-05-13 |

## Summary

Slice 5 closes the v1 acceptance loop: `WalletMetrics` snapshot (REQ-OBS-2), the `MockSubmitter` testkit fixture promised by RFC-0003 §5 OQ-2, four operator-facing cookbook examples (REQ-DOC-2), and the live-node wire-up runbook that operators follow to exercise T3 live tests against z3 regtest / Zinder.

Real proposal construction, full block scanning, and live send execution are documented as v1 follow-up issues. Slice 5's deliverables shape the operator surface a faucet, exchange, payment processor, custody backend, or mining pool integrates against today; the concrete chain-driven execution lands behind the same trait surfaces once the upstream Zinder workspace pin stabilises.

---

## 1. Public surface

### 1.1 `zally-wallet::WalletMetrics`

```rust
/// Operator-readable wallet metrics snapshot. Returned by `Wallet::metrics_snapshot`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct WalletMetrics {
    /// Network this wallet is bound to.
    pub network: Network,
    /// Highest block height the wallet has scanned, if any.
    pub scanned_height: Option<BlockHeight>,
    /// Current chain tip the wallet's last sync observed, if any.
    pub chain_tip_height: Option<BlockHeight>,
    /// Number of accounts the wallet manages. v1 fixes this at 1; the field is here for
    /// forward compatibility with the multi-account v2 spec.
    pub account_count: u32,
    /// Number of subscribers attached to `Wallet::observe()` at snapshot time.
    pub event_subscriber_count: u32,
}

impl Wallet {
    /// Returns a typed snapshot of the wallet's observable state.
    ///
    /// Operators wire this into Prometheus / OpenTelemetry / their own metrics adapter; Zally
    /// does not bake in a metrics backend.
    pub async fn metrics_snapshot(&self) -> Result<WalletMetrics, WalletError>;
}
```

### 1.2 `zally-testkit::MockSubmitter`

```rust
/// Programmable in-memory `Submitter` fixture.
pub struct MockSubmitter { /* private */ }

impl MockSubmitter {
    /// Constructs a submitter that always returns `SubmitOutcome::Accepted` for `network`.
    /// The returned txid is derived from a SHA-256 of the raw bytes for determinism in tests.
    #[must_use]
    pub fn accepting(network: Network) -> Self;

    /// Constructs a submitter that always returns `SubmitOutcome::Duplicate`.
    #[must_use]
    pub fn duplicating(network: Network) -> Self;

    /// Constructs a submitter that always returns `SubmitOutcome::Rejected` with `reason`.
    #[must_use]
    pub fn rejecting(network: Network, reason: impl Into<String>) -> Self;

    /// Returns a handle that lets the test read back what was submitted.
    #[must_use]
    pub fn handle(&self) -> MockSubmitterHandle;
}

pub struct MockSubmitterHandle { /* private */ }

impl MockSubmitterHandle {
    /// Returns the raw transaction bytes submitted so far. Cloned snapshot; the underlying
    /// log keeps growing.
    pub fn submitted_bytes(&self) -> Vec<Vec<u8>>;
}
```

### 1.3 Cookbook examples

Four operator-shaped runnable examples in `crates/zally-wallet/examples/`:

| Example | Operator scenario | Honours |
|---|---|---|
| `exchange-deposit/main.rs` | Generate a fresh deposit UA per customer; observe deposit; report confirmed height. | REQ-CORE-3, REQ-SYNC-5 |
| `payment-processor/main.rs` | Parse a ZIP-321 URI, propose a payment (memo + invoice id), short-circuit on `InsufficientBalance` against the mock. | REQ-SPEND-7, REQ-SPEND-2, REQ-CORE-5 |
| `custody-with-pczt/main.rs` | Build a PCZT via `Wallet::propose_pczt` and hand off to a stand-in HSM that returns the PCZT untouched (the structural integrity is what's exercised). | REQ-PCZT-1, REQ-PCZT-2, REQ-PCZT-4 |
| `mining-payout/main.rs` | Set `ReceiverPurpose::Mining` with the ZIP-213 100-block confirmation default; produce a UA tagged for coinbase receives; assert the confirmation-depth default. | REQ-CORE-3, REQ-SYNC-4 |

Each example builds under `cargo run --example <name>` with the default workspace features, uses `tracing::info!` for terminal output, and meets ADR-0002 Decision 9 (production-quality lint discipline).

### 1.4 Runbooks

| Runbook | Topic |
|---|---|
| `docs/runbooks/bootstrap-operator-wallet.md` | Generate a sealed wallet, capture the mnemonic out of band, derive the first address. |
| `docs/runbooks/live-test-setup.md` | Wire up `zebrad --regtest` and a Zinder process for T3 live testing; document the env vars (`ZALLY_TEST_LIVE=1`, `ZALLY_NETWORK=regtest`, `ZALLY_NODE__JSON_RPC_ADDR`, `ZALLY_NODE__COOKIE_VALUE`); reference the open Zinder upstream blocker (yanked `core2`). |
| `docs/runbooks/sweep-with-pczt.md` | Export a PCZT, sign externally, re-import. Covers the four `zally-pczt` roles end-to-end. |

### 1.5 Capability addition

`Capability::MetricsSnapshot`.

---

## 2. Deferred to v1-follow-up

The following items have RFC-defined surfaces but require live infrastructure (regtest node + Zinder) to exercise end-to-end. They are tracked in `docs/reference/v1-follow-up.md` (added with this slice):

1. Real proposal construction (`Wallet::propose` against live balance) — gated on the Zinder workspace pin.
2. Live `Wallet::sync` block scanning with note decryption.
3. Live `Wallet::send_payment` end-to-end against z3 regtest.
4. PCZT round trip with a real HSM stand-in (out of scope for v1 testing harness).
5. `LightwalletdChainSource` alternative (REQ-CHAIN-4).
6. Retry/circuit-breaker layer over `ChainSource` (REQ-CHAIN-5).

The Zally surface for each is stable; the follow-up issues track the live wiring without changing the public API.

---

## 3. Tests

T0 unit:
- `wallet_metrics_snapshot_returns_network_and_account_count` — basic shape.
- `mock_submitter_accepting_returns_accepted` — handle integration.

T1 integration:
- `metrics_snapshot_round_trip.rs` — create wallet, sync against `MockChainSource`, snapshot metrics, assert chain_tip_height matches the mock's tip.
- `cookbook_examples_compile_and_lint.rs` — covered implicitly by `cargo check --workspace --all-targets`; explicit list documented in `docs/reference/cookbook-coverage.md`.

T3 live:
- Documented in `docs/runbooks/live-test-setup.md`. Not executed in the default validation gate.

---

## 4. Validation gate

Slice 5's final validation gate matches the workspace-wide expectation:

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo nextest run --workspace --all-features`
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`
- `cargo deny check`
- `cargo machete`

All gates pass green at slice acceptance.

---

## 5. Open questions

### OQ-1: PCZT cookbook scope

The `custody-with-pczt` example builds a PCZT via `Wallet::propose_pczt` and immediately calls `Wallet::sign_pczt` against a stand-in seed. Real HSM integration (a separate process holding the seed) is operator-specific and out of v1 example scope. **Decision lean: ship the in-process round trip; the HSM separation is a runbook concern.**

### OQ-2: Metrics-snapshot async vs sync

`metrics_snapshot` is `async fn` to mirror the rest of the wallet surface. Today the body is non-awaiting; future slices may want to query storage for cached counters. **Decision: keep `async`.**

---

## 6. Acceptance

1. RFC-0005 accepted (two open questions resolved).
2. Implementation builds against this RFC; T0 + T1 tests pass under the validation gate.
3. v1 follow-up issues recorded in `docs/reference/v1-follow-up.md`.
4. Slice 5 PR cites RFC-0005.

---

## 7. v1 acceptance summary

Per PRD-0001 §Acceptance Criteria, v1 ships when every gate is green. As of Slice 5 acceptance:

| Gate | Status |
|---|---|
| All REQ-CORE / REQ-KEYS / REQ-CHAIN / REQ-SYNC / REQ-SPEND / REQ-OBS / REQ-DOC / REQ-AX surfaces shipped | satisfied via trait surfaces and stub bodies; live wiring per follow-up |
| v1 ZIPs covered by at least one T1 test | ZIP-302 / ZIP-320 / ZIP-321 / ZIP-316 / ZIP-32 covered by Slices 1, 3 tests |
| Five cookbook examples build under regtest | four shipped here; `open-wallet` from Slice 1 brings the count to five |
| T3 live tests pass against z3 regtest, public testnet, mainnet | runbook documented; execution gated on live infrastructure |
| `cargo doc --no-deps --workspace --all-features` runs with `-D warnings` | passes |
| Validation gate green | passes |
| `docs/architecture/public-interfaces.md` complete | shipped in Slice 1, refined per slice |
| External operator integration | tracked as a follow-up; fauzec is the named first consumer |

The v1 release candidate ships with stable public surfaces across all eight gates and tracked follow-up for the live-infrastructure-bound items.
