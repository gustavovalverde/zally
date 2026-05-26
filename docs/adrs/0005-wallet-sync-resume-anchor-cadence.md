# ADR-0005: Wallet Sync Resume — Anchor Cadence

| Field   | Value                                                                                                                                                                                                                                                                                                                              |
| ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status  | Accepted on 2026-05-26                                                                                                                                                                                                                                                                                                             |
| Product | Zally (decision applies to chain-source contract)                                                                                                                                                                                                                                                                                  |
| Domain  | Wallet sync resume, chain-source checkpoint policy                                                                                                                                                                                                                                                                                 |
| Related | [Public interfaces](../architecture/public-interfaces.md); [ADR-0001 Workspace crate boundaries](./0001-workspace-crate-boundaries.md); [fauzec platform-deduplication plan](https://github.com/gustavovalverde/fauzec/blob/main/docs/plans/2026-05-26-platform-deduplication.md) Phase 1 |

## Context

`Wallet::sync_inner` resumes scanning from the wallet's stored `fully_scanned_height`. `zcash_client_backend::data_api::chain::scan_cached_blocks` requires the cumulative `ChainState` at `scanned_from - 1` as input and panics with `assert_eq!(from_height, from_state.block_height() + 1)` when the supplied state does not match the requested resume point.

`zcash_client_backend-0.22.0` does not expose a public API for retrieving a `ChainState` from the wallet's own storage at an arbitrary previously-scanned height. `WalletRead::block_metadata` returns `BlockMetadata` (height, hash, tree sizes) but not the sapling/orchard frontiers required by `ChainState`. Reading the frontiers directly from `zcash_client_sqlite`'s commitment-tree tables would couple Zally to internal storage schema that changes between zcash_client_sqlite releases.

The wallet therefore depends structurally on the configured `ChainSource::tree_state_at` RPC for every sync resume. The performance cost is sub-percent (the upstream call is one in-process SQLite read on the indexer plus a unary gRPC roundtrip; compact-block scanning dominates). The correctness cost is non-trivial: when the chain source's tree-state checkpoint set is sparser than the wallet's scan position, `tree_state_at(prior_height)` returns a state at some lower height `K < prior_height`. `Wallet::realign_to_available_checkpoint` then truncates the wallet to `K` and rescans forward. `zcash_client_sqlite::truncate_to_height` enforces a roughly 100-block rewind cap (`COINBASE_MATURITY` + safety margin); any realign target below that cap fails `NotRetryable` and the wallet wedges until a manual reset.

The fauzec faucet wedged this way on 2026-05-26 at 04:18 UTC. The wallet's `fully_scanned_height` was 4028669 and zinder's nearest at-or-below checkpoint was at 4028000, 669 blocks below the rewind cap. Recovery required wiping the wallet DB and resyncing from birthday.

## Decision

1. **The chain-source contract for `tree_state_at(h)` requires a checkpoint within the wallet's rewind window of `h`.** The wallet honors a 100-block rewind cap (the librustzcash default; not configurable from this side). Chain sources serving Zally guarantee that for any height `h` they have ingested, `tree_state_at(h)` returns a state at some height `K` such that `h - K < 100`.

2. **The fauzec-supplied chain source (`zinder`) meets the contract by writing tree-state checkpoints at a fixed stride.** Both ingest phases — bulk-catchup and tip-follow — emit one checkpoint every 100 blocks (`zinder-ingest::chain_ingest::TREE_STATE_CHECKPOINT_STRIDE`). A tip-follow batch during chain catchup spans tens of blocks; without the stride, a single-end-of-batch checkpoint left gaps wide enough to wedge the wallet, which is exactly what produced the 04:18 incident.

3. **The wallet does not cache chain-source responses on its own side.** A consumer-side cache was prototyped and reverted: the cache hit rate is structurally zero in steady state because every successful sync advances `fully_scanned_height`, so the cached row's height (the previous cycle's `prior_height`) never matches the next cycle's `prior_height`. Caching at the post-scan height would require an additional `tree_state_at` RPC per cycle, exchanging one RPC for one cache write with no net dependency reduction. The producer-side stride decision (above) achieves the actual goal (the wedge class disappears) without the cache machinery.

4. **`realign_to_available_checkpoint` remains in `Wallet::sync_inner` as defense in depth.** With the producer-side stride, the realign path fires only in degenerate cases: a custom chain source that does not honor the cadence contract, a chain-source bug that under-writes checkpoints, or a deployment where the wallet's `fully_scanned` drifts above any available checkpoint after a multi-day stall. The realign is small, well-tested, and its failure mode (`NotRetryable`) surfaces clearly in the observability stream.

5. **The cadence is encoded as a single workspace-level constant in zinder.** `chain_ingest::TREE_STATE_CHECKPOINT_STRIDE = 100` is the only place the number lives. Both `populate_bulk_catchup_tree_state_checkpoint` and `populate_tip_follow_tree_state_checkpoint` import from there; the bulk-catchup ADR comment and this ADR cite the same constant. Lifting the stride to a wallet-configurable knob is rejected: the contract is between Zally and the chain source, not the operator, and an operator that ships a smaller stride only hurts ingest throughput without changing the wedge class.

## Consequences

- The wedge class observed on 2026-05-26 04:18 UTC is closed. The cadence guarantees an at-or-below checkpoint always exists within the rewind cap for any height a wallet has scanned past.
- Tip-follow incurs one additional `z_gettreestate` upstream RPC per stride boundary in each commit batch. For testnet at 75-second blocks the amortized cost is one extra RPC per ~125 minutes per batch boundary; absolute cost on the upstream Zebra node is negligible.
- The wallet sync path stays simple: one `tree_state_at` RPC per resume, one optional realign on a checkpoint mismatch, one scan. No persistence layer between the chain source and `scan_cached_blocks`.
- Operators that ship custom chain sources must honor the cadence contract or accept that wallet recovery may require a manual reset. The contract is documented; the failure surface is observable.

## Alternatives considered

- **Consumer-side anchor cache in zally.** Implemented and reverted. Cache hit rate is structurally zero in steady state because `scan_cached_blocks` requires an exact-match `ChainState` at `prior_height` and every successful sync moves `prior_height` forward by the scanned-blocks count, ahead of any height the cache has captured. The cache write costs one row per sync; the cache read returns at-or-below results that are usable only at exact match, and exact match never lands on a row the previous sync wrote.
- **Reading commitment-tree frontiers directly from `zcash_client_sqlite` internal tables.** Rejected: tight coupling to a private schema. Every `zcash_client_sqlite` minor bump would risk breaking Zally; the maintenance burden outpaces the dependency reduction.
- **Petitioning `zcash_client_backend` upstream to add `WalletRead::chain_state_at(h)`.** The right long-term move; deferred. The ecosystem timeline is months. The producer-side stride decision unblocks Zally users today without preventing the upstream improvement later.
- **Stride lower than 100 blocks (e.g. 50).** Rejected: gains nothing because the librustzcash rewind cap is 100. A smaller stride pays one extra RPC per stride-divisor without reducing the wedge surface.
- **Stride higher than 100 blocks (e.g. 200).** Rejected: violates the cadence contract. Any height `h` would have an at-or-below checkpoint up to 200 blocks behind, exceeding the rewind cap.
