# Proposal 0002: Pending-broadcast awareness for the shielding input selector

| Field        | Value                                                                                                                                |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------ |
| Status       | Proposed                                                                                                                             |
| Product      | Zally                                                                                                                                |
| Domain       | `zally-wallet` shielding entrypoint, `zally-storage` transparent UTXO read surface                                                   |
| Consumer     | [fauzec](https://github.com/ZcashFoundation/fauzec) (testnet faucet)                                                                 |
| Pinned at    | `zally-wallet` rev `ba15aa307bd6a2cbabab6e0c0ad5d0451dfed779`                                                                        |
| Related      | [Proposal 0001](0001-wallet-read-surface-for-fauzec-auto-shield.md), [Public interfaces](../architecture/public-interfaces.md)        |

## Context

Fauzec's auto-shield loop (ADR-0006 in the consumer repo) calls `Wallet::shield_transparent_funds` on a 10-minute cadence. The first cycle after a fresh deploy broadcasts a sweep of every mature coinbase UTXO at the wallet's transparent receiver into an Orchard change output. The sweep on production testnet was 620 transparent inputs and confirmed at the very next block (`a00568104816fa6aafbaec4d70e56993f01ab73893a8d1430a59cc94620debd2` in block `000eb06604d3c944845e332850d93e0aa2a5550bfbff6758667fd0c6cdceeb66` at height 4017462). The wallet's reported spendable shielded balance climbed to roughly 395 ZEC within seconds.

Cycle two fired ten minutes later. It failed with:

```
auto_shield_failed
failure="UnexpectedReply(\"proposal rejected: rejected (-1): transaction dropped because it is already queued for download\")"
```

That string maps directly to Zebra's `MempoolError::AlreadyQueued` (`zebra/zebrad/src/components/mempool/error.rs:61`). The condition fires when a `sendrawtransaction` submission carries a txid that Zebra has already accepted into its download pipeline, either because a peer gossiped the same txid via INV or because our own previous broadcast is still circulating. The byte-identical replay only happens when the input selector reuses the same 620 transparent outpoints, the change strategy targets the same Orchard internal address, and the proposal layer reuses the same anchor. With deterministic inputs, those three together produce a deterministic txid.

The root cause is downstream of the consumer: between the moment the wallet broadcasts a shielding tx and the moment its sync loop observes the spend in a confirmed block, `WalletStorage::get_spendable_transparent_outputs` still returns the outpoints the prior broadcast already consumed. The shielding entrypoint runs again, reselects the same inputs, rebuilds the same tx, and Zebra rejects it.

Every subsequent cycle repeats the failure until the sync loop catches up. Each failure increments `fauzec_wallet_auto_shield_attempts_total{outcome="failed"}` and skips the liveness `mark_progress()` call. If the failures span the full stall threshold (`4 * interval_seconds + SHIELD_TIMEOUT` in fauzec), the consumer's readiness gate flips to `WalletUnavailable` even though the funding plane has live shielded balance to spend.

This is not the librustzcash `ChangeRequired` fee-disagreement bug ([upstream #2346](https://github.com/zcash/librustzcash/issues/2346)). That one already shipped in `zcash_primitives 0.27.1` and fauzec pulled it before the cycle-1 broadcast succeeded. `AlreadyQueued` is structurally different: it is a sync-lag issue, not a fee-math one.

## Goals

- Make `Wallet::shield_transparent_funds` exclude transparent outpoints that the wallet has already spent in a still-unconfirmed broadcast. The next cycle then operates on a different input set, produces a different txid, and clears the duplicate-rejection class entirely.
- Expose the same information through a read API so consumers can short-circuit their own cycles (skip a tick, log a structured `pending_in_flight` event) without waiting for a chain round-trip.
- Keep the change scoped to shielding. Send-path input selection (`Wallet::send_payment`) already filters against the wallet's pending-spend set via a different code path; we are closing the analogous gap on the shielding path.

## Non-goals

- No change to `ShieldOutcome`, no new variant. The existing `Broadcast` and `BelowThreshold` are sufficient. The new behaviour shows up as more frequent `BelowThreshold` cycles while a prior broadcast is in flight, which is the correct semantic.
- No new confirmations policy. Mature transparent maturity stays at 100 blocks per Zcash protocol; this proposal only changes which mature outpoints are eligible as inputs, not the maturity definition.
- No new persistence shape. The "pending broadcast" set already lives in `WalletStorage` rows that record sent transactions; this proposal surfaces them rather than redefining them.
- No upstream librustzcash change. The proposal is local to Zally's storage and wallet crates and composes the existing `WalletStorage` read methods.

## Proposed changes

### 1. `WalletStorage::get_spendable_transparent_outputs` excludes pending broadcasts

The storage method that backs `ShieldingSelector::propose_shielding` already accepts a transparent receiver, a `target_height`, and a `ConfirmationsPolicy`. Extend its implementation to also filter out any outpoint that appears as an input on a wallet-owned transaction whose `mined_height` is `NULL` and whose broadcast timestamp is within a configurable inflight window (default 2 hours).

The filter lives entirely in storage; no callers need to change their call sites. Send-path input selection gets the same protection for free because both paths go through the same storage method.

Naming notes: the existing public signature stays as-is. The behaviour change is documented as a contract refinement under `WalletStorage::get_spendable_transparent_outputs`: "outpoints belonging to wallet-owned broadcasts that have not yet been observed as mined are excluded from the result set". Existing T1 tests that only ever broadcast once continue to pass.

### 2. `Wallet::pending_outgoing_transparent_inputs(account_id) -> PendingTransparentInputs`

```rust
impl Wallet {
    /// Returns the transparent outpoints currently locked by a wallet-owned
    /// transaction that has been broadcast but not yet observed mined.
    ///
    /// `not_retryable` on unknown account; `retryable` on transient storage I/O.
    pub async fn pending_outgoing_transparent_inputs(
        &self,
        account_id: AccountId,
    ) -> Result<PendingTransparentInputs, WalletError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PendingTransparentInputs {
    pub network: Network,
    pub account_id: AccountId,
    pub entries: Vec<PendingTransparentInput>,
    /// Persisted visible tip the snapshot is anchored to. `None` when the
    /// wallet has not yet recorded a tip.
    pub as_of_height: Option<BlockHeight>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PendingTransparentInput {
    pub outpoint: OutPoint,
    pub value_zat: Zatoshis,
    pub broadcast_txid: TxId,
    pub broadcast_at_ms: u64,
}
```

This is the read-side counterpart of change 1. A custody dashboard, an auto-shield loop, or a metrics exporter can call it to decide whether to skip a cycle entirely. The wire shape mirrors `AccountBalance` from Proposal 0001: every record carries the `network` tag from the spine and the `_zat` / `_ms` / `_height` suffix discipline.

### 3. Structured event when the storage filter trims outpoints

When `get_spendable_transparent_outputs` excludes one or more outpoints because of a pending broadcast, emit a single tracing event per call at `info`:

```
tracing::info!(
    event = "transparent_inputs_filtered_pending_broadcast",
    network = %network,
    account_id = %account_id,
    excluded_count = ...,
    pending_broadcast_count = ...,
);
```

Without this event, operators have no signal that the filter is active. With it, the `auto_shield_below_threshold` log line that follows a pending broadcast carries a clear "this is why" predecessor in the timeline.

## Why a consumer-only workaround does not fit

Fauzec considered three workarounds before requesting this PRD:

- **Track the last broadcast txid in the runtime and skip cycles whose proposal hashes to the same id.** Requires the consumer to compute the proposal, hash it, compare, and discard the result. Wastes one proof-and-sign cycle per failed attempt (tens of seconds of CPU for an Orchard sweep) and re-implements logic that storage already owns.
- **Increase the cycle interval until the sync loop is guaranteed to have caught up.** Trades operational latency for false safety. The sync loop is not bounded by a known SLA; making the auto-shield interval longer just makes the failure mode less frequent, not absent.
- **Manually mark the broadcast outpoints as spent in the consumer's own state.** Cannot be done without reaching into Zally's `WalletStorage`, which violates the encapsulation Zally is built to enforce (ADR-0001).

The right fix is in Zally because Zally already knows which outpoints its broadcasts locked. The information is in `WalletStorage` rows; it is just not yet a filter.

## Acceptance criteria

A change shipping under this proposal must:

1. Refine `WalletStorage::get_spendable_transparent_outputs` to exclude outpoints belonging to wallet-owned broadcasts whose `mined_height` is still `NULL`, with a configurable inflight window (default 2 hours) so a permanently-dropped broadcast eventually frees its outpoints.
2. Add `Wallet::pending_outgoing_transparent_inputs`, `PendingTransparentInputs`, and `PendingTransparentInput` to `zally-wallet`, with a rustdoc example and at least one T1 integration test that asserts the snapshot reflects a just-broadcast tx and clears once the tx is observed mined.
3. Add the structured tracing event `transparent_inputs_filtered_pending_broadcast` on the storage path that performs the filter.
4. Add a T2 live-CI test against `z3` regtest that broadcasts a shielding sweep, immediately calls `shield_transparent_funds` again, and asserts the second call returns `ShieldOutcome::BelowThreshold` instead of attempting a duplicate broadcast.
5. Pass the standard validation gate (`cargo fmt --check`, `cargo check --workspace --all-targets --all-features`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo nextest run --profile=ci`, rustdoc with `-D warnings`, `cargo deny`, `cargo machete`).
6. Be additive: existing callers of `Wallet::shield_transparent_funds` and `Wallet::send_payment` see no signature change. The new behaviour is a stricter input filter, not a new contract surface.
7. Update the public-interfaces spine with the new verb and types under the existing `pending_*` / read row, and update the SPEND-* invariant numbering to record the new storage filter contract.

## Open questions

- What is the right default for the inflight window? 2 hours covers Zcash testnet block times comfortably (40 blocks at 5 min each), and would not strand outpoints longer than a typical operator notices a problem. Mainnet at 75 s blocks could go shorter (30 min). Default position: 2 hours, configurable per wallet open via `WalletOptions`.
- Should `PendingTransparentInputs::entries` include outpoints from `send_payment` broadcasts too, not just shielding? Default position: yes, this is a storage-level snapshot of "what is locked", not a path-specific view.
- Should the storage filter also exclude outpoints belonging to broadcasts that have been observed in mempool but not yet mined (Zebra `getrawmempool` membership)? Default position: no, that would couple storage to a chain-source RPC. The broadcast-timestamp window is sufficient; sync will mark the inputs spent once the tx confirms.
- Should the wallet emit a Recoverable warning (not error) when `shield_transparent_funds` is called while a prior broadcast is in flight and the filter produced no eligible inputs? Currently the contract returns `ShieldOutcome::BelowThreshold` silently; explicit feedback could help consumers reason about state. Default position: rely on the tracing event for now, add to `ShieldOutcome` only if a consumer asks.

## Downstream impact

- **fauzec**: cycle-2-and-onward duplicate-broadcast failures disappear. The auto-shield loop's `BelowThreshold` rate goes up for a short window after every Broadcast, then falls back to its steady state once new mature transparent value accumulates. The consumer's `/readyz` stall protection stops being triggered by a sync-lag race. The follow-up task tracking this issue in fauzec (currently captured in the consumer's memory under `reference_shield_duplicate_txid_already_queued`) closes.
- **Other custody integrators**: anyone who calls `shield_transparent_funds` on a cadence faster than a confirmation interval inherits the same protection without writing new code. The pending-broadcast snapshot also gives them a primitive to build operator-facing "we have an unconfirmed sweep in flight" dashboards.
- **Signer-only services**: no impact. The change is on `zally-wallet` and `zally-storage`; signer-only paths do not depend on either per ADR-0001.
