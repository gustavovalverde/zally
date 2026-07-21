# ADR-0005: Wallet Sync via Canonical Scan-Range Orchestration

| Field   | Value                                                                                                                                                                                                       |
| ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status  | Accepted on 2026-05-26; revised 2026-05-27 (live-tip scan ceiling); revised 2026-06-03 (canonical scan-range orchestration replaces the hand-rolled resume loop); revised 2026-07-21 (pinned visible-tip scan target) |
| Product | Zally (decision applies to the chain-source contract and the sync loop)                                                                                                                                    |
| Domain  | Wallet sync planning, subtree-root priming, anchor selection, reorg recovery, tx-expiry math                                                                                                               |
| Related | [Public interfaces](../architecture/public-interfaces.md); [ADR-0001 Workspace crate boundaries](./0001-workspace-crate-boundaries.md); [ADR-0002 Source failure posture](./0002-source-failure-posture.md) |

## Context

`zcash_client_backend` ships a documented sync protocol (`data_api::chain` module docs): push commitment-tree subtree roots, call `update_chain_tip`, ask the wallet for work with `suggest_scan_ranges`, scan the `Verify` range first, then the rest, fetching the chain state for `range.start - 1` before each `scan_cached_blocks`. The protocol exists so the `from_height == from_state.block_height() + 1` precondition of `scan_cached_blocks` holds by construction, and so notes become witness-anchored (spendable) from subtree roots without scanning every historical block.

The first two revisions of this ADR hand-rolled the loop instead. The wallet computed `scanned_from = fully_scanned_height + 1` itself and fetched `from_state` from zinder's sparse tree-state checkpoints (one per ~100 blocks). It never primed subtree roots. Two failures followed:

- **Spendable balance zeroed.** With no subtree roots, the sqlite backend refuses to report a note spendable while the shard containing the ZIP-315 anchor has unscanned ranges at or below the anchor. The 2026-05-27 revision worked around this by scanning all the way to the live tip so the anchor always landed in fully-scanned territory; that forced a full linear birthday-to-tip scan.
- **The scan panicked and wedged.** `from_state` came from a checkpoint at-or-before `scanned_from - 1`, almost never exactly `scanned_from - 1`, so `scan_cached_blocks` hit its `assert_eq!` and panicked. The panic killed the single-threaded wallet-db actor, freezing the faucet ~11k blocks behind the tip.

Both are downstream of one decision: hand-rolling the loop instead of using the library protocol. zinder now serves a coherent tree state at any exact height (zinder ADR-0005, tree-state-at-height carve-out), which removes the last reason the loop had to be hand-rolled.

## Decisions

1. **Sync is driven by the library's scan-range orchestration.** Each `Wallet::sync` call: (a) primes subtree roots for both pools via `WalletStorage::put_subtree_roots`; (b) calls `update_chain_tip(live_tip)`; (c) asks `WalletStorage::suggest_scan_ranges` for the highest-priority range; (d) scans one chunk of it (bounded by `MAX_BLOCKS_PER_SYNC`) with `from_state` fetched at exactly `range.start - 1`. The `SyncDriver` loops `Wallet::sync` until the scan queue drains. The hand-rolled `scanned_from` arithmetic, the birthday fallback, and the linear single-range fetch are deleted.

2. **Subtree-root priming is mandatory and runs before planning.** `ChainSource::subtree_roots` carries each root's `completing_block_height`; the wallet records it through `put_subtree_roots` for every shielded pool so a note is witness-anchored from its subtree root without scanning the whole subtree. This is part of the spendable-zero fix: completed shards do not require a full linear scan, and `suggest_scan_ranges` can plan subtree-aligned work.

3. **Each sync attempt scans through one pinned epoch's visible tip.** `ChainSource::current_epoch()` returns visible and settled identities atomically. The wallet passes that epoch to every compact-block, tree-state, subtree-root, and transparent-UTXO read, calls `update_chain_tip` with its visible tip, and clamps scan ranges to that same visible height. This keeps the upstream wallet tip and scan queue aligned. Leaving the visible-to-settled gap unscanned can make an otherwise mature note unspendable when both the note and that gap occupy the same incomplete commitment-tree shard. Zinder's epoch pin makes every artifact in the settlement window immutable for the attempt; a later reorg is handled by Verify-range scanning and the repair ladder. The settled tip remains finality metadata for settlement-sensitive policy. Because zinder serves an exact tree state at `range.start - 1`, `from_height == from_state.block_height() + 1` holds for every `scan_cached_blocks` call and the upstream assert can no longer fire.

4. **Reorg recovery is the library's Verify-range mechanism plus precise truncation.** `WalletStorage::scan_blocks` surfaces `StorageError::ChainReorgDetected { at_height }` (`Retryable`) on a continuity error. The error reaches the `SyncDriver`, whose repair ladder rewinds: it fetches the chain state at the current scanned height minus the active `REWIND_LADDER_BLOCKS` depth (10, then 100) and calls `WalletStorage::truncate_to_chain_state`, which lands the wallet at exactly that height; the next sync re-plans via `suggest_scan_ranges`. The `Verify`-priority range that `suggest_scan_ranges` returns first proactively re-validates the tip-adjacent blocks each cycle. The hand-rolled tip-regress detector, the `truncate_to_height`-based rewind (which snapped down to the nearest checkpoint and spiralled the scan backwards), and the rewind-margin arithmetic are deleted. `truncate_to_height` is removed from the storage trait. Divergences a 100-block rewind cannot clear (the librustzcash rewind cap, `COINBASE_MATURITY`) escalate to a rebuild from the birthday via `Wallet::reset_to_birthday`; no operator reset is required.

5. **An expired epoch restarts the whole sync attempt.** Zinder reports `ChainEpochPinUnavailable` when it can no longer serve the requested immutable view. The client classifies this as `RefreshChainEpoch`; Zally discards the partial attempt, acquires a new epoch, and restarts every artifact read under that epoch. It never retries an artifact with the stale pin or mixes artifacts from two epochs.

6. **The wallet-db actor `catch_unwind` is defense-in-depth, not the scan-continuity safety net.** With the canonical loop the continuity assert can no longer fire from the scan path, so the actor's panic isolation is a generic last-resort backstop; the supervisor watchdog remains the recovery boundary.

## Consequences

- The `spendable_zat: 0` wedge is closed at its root: completed subtree roots witness-anchor historical notes, while scanning through the pinned visible tip removes unscanned gaps from an incomplete tip shard. `get_wallet_summary(ConfirmationsPolicy::default()).spendable_value()` reports the real balance once the note has enough confirmations.
- The `scan_cached_blocks` panic is closed at its root: every `from_state` is fetched at the exact `range.start - 1` the wallet asked for, so the invariant holds by construction.
- Catch-up is faster: the wallet scans the tip-adjacent `Verify`/`ChainTip` ranges first and witnesses history from subtree roots, so spendability arrives before the full history is linearly scanned.
- The `BadExpiryHeight` class stays closed: `update_chain_tip` is the live head, so the proposal builder's target is `head + 1` and expiry sits above the head.
- Reorgs are auto-recovered via precise truncation; operators see `WalletEvent::ReorgDetected`. Deep reorgs (> 100 blocks) remain operator-visible; resetting `wallet.db` (preserving sealed seed material) is the recovery path.

## Operator runbook

`StorageError::ChainReorgDetected` is `Retryable` and recovered in-loop; it should not surface except in metrics. If the wallet wedges on a `truncate_to_chain_state` failure (deeper-than-cap reorg) the operator sees `wallet_sync_snapshot_error` with posture `NotRetryable`:

1. Confirm the chain source is healthy. Query the current chain epoch from zinder and verify that both its visible and settled tips advance.
2. Reset `wallet.db` while preserving sealed key material (`wallet.age`, `wallet.age.age-identity`). The wallet re-scans from birthday; subtree roots are re-primed on the first sync.
3. The circuit breaker is not involved: `NotRetryable` postures are excluded from breaker counting. Other wallet operations remain available; only sync is paused.

## Alternatives considered

- **Keep hand-rolling the loop and only fix `from_state` alignment.** Rejected: it would not fix the spendable-zero wedge (no subtree roots) and would keep re-deriving work the wallet's own scan queue already plans. The library protocol is the pattern other wallets copy; diverging from it is the entropy this revision removes.
- **Scan only through the settled tip.** Rejected: `update_chain_tip(visible_tip)` then leaves the visible-to-settled gap in the upstream scan queue. If that gap shares an incomplete commitment-tree shard with a mature note, the shard remains above `ScanPriority::Scanned` and the upstream balance query reports the note as unspendable. A pinned `ChainEpoch` already prevents artifacts from changing within one attempt; later canonical replacement is handled by Verify-range scanning and reorg recovery.
- **Adopt the full async `sync` feature / `BlockCache` machinery.** Deferred: the in-process single-chunk fetch plus `suggest_scan_ranges` is sufficient and smaller. The minimum correct design is subtree priming + suggest-driven ranges + exact-boundary `from_state` + `Verify`-first ordering + `truncate_to_chain_state`.
