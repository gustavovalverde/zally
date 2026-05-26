# ADR-0005: Chain-State Anchor Cache

| Field   | Value                                                                                                                                                                                                                                                                                                                              |
| ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status  | Accepted on 2026-05-26                                                                                                                                                                                                                                                                                                             |
| Product | Zally                                                                                                                                                                                                                                                                                                                              |
| Domain  | Wallet sync, storage migrations, chain-source coupling                                                                                                                                                                                                                                                                             |
| Related | [Public interfaces](../architecture/public-interfaces.md); [ADR-0001 Workspace crate boundaries](./0001-workspace-crate-boundaries.md); [fauzec platform-deduplication plan](https://github.com/gustavovalverde/fauzec/blob/main/docs/plans/2026-05-26-platform-deduplication.md) Phase 1 |

## Context

`Wallet::sync_inner` resumes scanning from the wallet's stored `fully_scanned_height`. `scan_cached_blocks` requires the cumulative `ChainState` (sapling and orchard frontiers) at `scanned_from - 1` as input. The wallet's own SQLite database holds the per-pool shardtree state, but `zcash_client_backend-0.22.0::data_api` does not expose a public method to extract a `ChainState` at an arbitrary scanned height; the frontiers are only reachable through `scan_cached_blocks`' internal `prior_block_metadata` loop.

Lacking that API, every sync cycle calls `chain.tree_state_at(scanned_from - 1)` on the configured chain source. The contract makes the wallet's progress depend on the chain source's tree-state checkpoint retention and cadence policies:

- A chain source that prunes a checkpoint the wallet has scanned past wedges the wallet's resume path. fauzec observed this in production on 2026-05-26 when zinder's bulk-catchup cadence and the wallet's `fully_scanned_height` drifted apart by more than the librustzcash rewind cap; only a wallet wipe recovered.
- The realign code (`Wallet::sync_inner::realign_to_available_checkpoint`) handles the case where the chain source's nearest at-or-below checkpoint is sparser than the wallet's scan position. When the realign target is also below librustzcash's rewind cap, the truncate fails `NotRetryable` and the wallet stays stuck.
- Every healthy sync issues one RPC for tree state. The wallet pays a network roundtrip for state it has, structurally, already integrated.

The fully self-sufficient form (wallet extracts its own `ChainState` from local storage and stops calling the chain source for resume state) requires an upstream `WalletRead::chain_state_at(height)` method that does not exist in zcash_client_backend. The achievable form caches every `ChainState` the wallet receives so the next resume hits the wallet's own database instead of repeating the RPC.

## Decision

1. **The wallet caches every `ChainState` it receives from the chain source.** A new `ext_zally_chain_state_anchors` table is created at `WalletStorage::open_or_create` time alongside the other `ext_zally_*` tables. Each row stores the height, the opaque `zcash_client_backend::proto::service::TreeState` proto-encoded bytes the wallet received, and the wall-clock unix milliseconds at which the row was written. `block_height` is the primary key; the encoded payload is the same byte sequence the chain source's gRPC RPC returns, so cached rows round-trip through the existing `TreeState::to_chain_state` decoder with no parallel format.

2. **Resume consults the cache first.** `Wallet::sync_inner` calls `WalletStorage::find_chain_state_anchor_at_or_below(scanned_from - 1)` before any chain-source RPC. A hit at exactly `scanned_from - 1` is returned as the resume state; a hit at a lower height is unusable for the resume assertion (`scan_cached_blocks` requires `from_height == from_state.block_height + 1`) and is treated as a miss. Misses fall back to the chain source's `tree_state_at` RPC, and the response is recorded in the cache before being returned.

3. **The chain-source role contracts to "first call, then fallback."** A fresh wallet at birthday calls the chain source exactly once; from that point forward, every healthy resume hits the cache. The chain source remains the bootstrap and the fallback for the sparse-checkpoint case (the realign path covered in `sync_inner`), but the routine path no longer depends on it.

4. **The cache is a write-through layer, never the only copy.** Cached bytes that fail to decode (on-disk corruption, format drift) surface as `wallet_chain_state_anchor_decode_failed` warnings and force a fall-through to the chain source. The corrupted row is pruned in the same call so subsequent resumes do not loop on it.

5. **Retention is bounded by a constant block window.** Each successful write prunes rows strictly below `recorded_height - 1000` (constant `CHAIN_STATE_ANCHOR_RETENTION_BLOCKS`). 1000 blocks comfortably covers the librustzcash rewind cap plus any reasonable operator stall window; rows older than that cannot satisfy a resume request and accumulating them would waste sqlite pages without changing any observable behavior. Retention is enforced lazily (on next write) rather than on a schedule.

6. **`WalletStorage::find_chain_state_anchor_at_or_below` returns the highest at-or-below row, not the exact-match row.** The wallet's caller checks for exact match before using the row. The at-or-below shape leaves room for a future refinement that uses a slightly-older anchor with a forward replay, without changing the trait surface.

7. **Encoding is the upstream `TreeState` proto.** The cache stores the same bytes the chain source's gRPC RPC returns, so the wallet's decoder (`zcash_client_backend::proto::service::TreeState::decode` followed by `to_chain_state()`) handles both cached and live state through the same path. The storage crate does not interpret the payload.

## Consequences

- The wallet's healthy-resume path drops one network roundtrip per sync cycle. A faucet syncing every 60 seconds saves 60 RPCs per hour, with no operational change beyond the new ledger table.
- The wallet's resume tolerates chain-source checkpoint pruning. As long as the wallet had a successful sync within the retention window, a chain source that has since pruned the matching checkpoint is no longer load-bearing.
- The cache cannot retire the realign code path entirely. Cold-start cases (a fresh wallet or a wallet that stalled longer than the retention window) still hit the chain source and may still receive a state at a lower height than requested. The wallet's behavior in that case is unchanged from before this ADR; the cache simply makes it rare.
- The `ext_zally_chain_state_anchors` table grows at one row per successful chain-source RPC. With the retention prune, steady-state storage cost is bounded at `RETENTION_BLOCKS × ~16KB ≈ 16MB` per wallet, which is negligible against the wallet DB size.
- Observability gains three new events: `wallet_chain_state_anchor_cache_hit`, `wallet_chain_state_anchor_cached` (on successful record), and `wallet_chain_state_anchor_decode_failed` (corruption detected). Operators reading the event stream can verify the cache is doing its job by counting hits.

## Alternatives considered

- **Implement a parallel commitment-tree maintenance loop alongside librustzcash.** Rejected: maintaining shielded note commitment trees from compact blocks is a non-trivial cryptographic responsibility; duplicating librustzcash's implementation would split the consensus surface across two implementations, both of which would need to be kept in lockstep with upstream protocol upgrades.
- **Read the wallet's per-pool shardtree tables directly via raw rusqlite.** Rejected: those tables are zcash_client_sqlite implementation details. The schema can and does change between minor releases; coupling Zally's correctness to that schema would create breakage every time we bump the upstream pin.
- **Push for a `WalletRead::chain_state_at(height)` API upstream.** Deferred: the right long-term answer, but it is months out and requires consensus across the broader Zcash wallet ecosystem. The anchor cache is the right interim shape regardless: even with that API, callers caching the result avoid repeated reads.
- **Cache at a fixed stride (every N blocks) rather than at every received height.** Rejected: the wallet only receives state at heights the chain source returns (typically `scanned_from - 1` per resume). Caching at a stride would either drop responses the wallet already paid for, or require additional RPCs to fill the stride. One row per RPC is the cheapest contract that satisfies the resume invariant.
