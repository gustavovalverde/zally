//! Wallet sync loop.
//!
//! `Wallet::sync` drives `zcash_client_backend::data_api::chain::scan_cached_blocks` through
//! the storage-side `WalletStorage::scan_blocks` extension. The chain source streams compact
//! blocks; the wallet drains them, builds a `ChainState`, and hands both to the storage layer,
//! which runs the upstream scanner against the live `WalletDb`.
//!
//! Each call re-scans from the wallet's last fully-scanned height up to the current chain
//! tip. The `ChainState` is rebuilt from the embedded genesis frontier on every call: correct,
//! linear-in-tip-height.

use std::collections::HashMap;

use futures_util::StreamExt as _;
use zally_chain::{BlockHeightRange, ChainSource, ChainSourceError, ChainState, ShieldedPool};
use zally_core::BlockHeight;
use zally_storage::{ScanRequest, StorageError};

use crate::event::WalletEvent;
use crate::retry::with_breaker_and_retry;
use crate::wallet::Wallet;
use crate::wallet_error::WalletError;

const MAX_BLOCKS_PER_SYNC: u32 = 1_000;

/// Blocks to roll back past a divergence reported by `scan_cached_blocks`.
///
/// Must exceed the chain source's reorg window so recovery always makes forward progress,
/// even when the divergence is detected at `fully_scanned_height + 1` (where an
/// `at_height - 1` rollback would be a no-op). 160 blocks leaves comfortable margin above
/// typical Zcash reorg windows (100 blocks for shielded coinbase maturity, lower in
/// practice on testnet and regtest).
const REORG_ROLLBACK_DEPTH_BLOCKS: u32 = 160;

struct ScanContext {
    blocks: Vec<zcash_client_backend::proto::compact_formats::CompactBlock>,
    scanned_from: BlockHeight,
    target_height: BlockHeight,
    block_count: u64,
    reorgs_observed: u32,
}

/// Summary of a `Wallet::sync` run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SyncOutcome {
    /// Height the wallet was scanned from (exclusive of the prior scan progress).
    pub scanned_from_height: BlockHeight,
    /// Height the wallet finished scanning at.
    pub scanned_to_height: BlockHeight,
    /// Number of blocks scanned during this run.
    pub block_count: u64,
    /// Number of reorgs observed during this run.
    pub reorgs_observed: u32,
}

impl Wallet {
    /// Advances the wallet from its last-scanned height up to `chain.chain_tip()`.
    ///
    /// Scanning reaches the chain tip so the commitment tree, note witnesses, and the
    /// WalletDb chain-tip notion all agree: `zcash_client_backend` only treats a note as
    /// spendable when its witness is anchored within a fully-scanned tip, and transaction
    /// expiry heights are computed against that same tip. Reorg safety comes from the
    /// spend-time confirmation depth (ZIP 315) and from `roll_back_after_reorg` recovery, not
    /// from withholding recent blocks from the scan.
    ///
    /// Fails closed on network mismatch. Emits `ScanProgress` events at the start and end of
    /// the run; per-block events are emitted by the storage scanner.
    ///
    /// `not_retryable` on network mismatch. `retryable` on transient chain-source failures.
    pub async fn sync(&self, chain: &dyn ChainSource) -> Result<SyncOutcome, WalletError> {
        if chain.network() != self.network() {
            return Err(WalletError::NetworkMismatch {
                storage: self.network(),
                requested: chain.network(),
            });
        }
        let policy = self.retry_policy();
        let chain_tip = with_breaker_and_retry(
            &self.inner.circuit_breaker,
            policy,
            "sync.chain_tip",
            || chain.chain_tip(),
            |e| map_chain_source_error(&e),
        )
        .await?;
        let prior_observed_tip = self.inner.storage.lookup_observed_tip().await?;
        let reorg = self.detect_tip_regress(prior_observed_tip, chain_tip);
        self.inner.storage.record_observed_tip(chain_tip).await?;
        // Pin the WalletDb chain tip to the height this run scans to, so transaction
        // proposals compute expiry and anchor heights against a fully-scanned tip.
        self.inner.storage.update_chain_tip(chain_tip).await?;

        let prior_fully_scanned_height = self.inner.storage.fully_scanned_height().await?;
        let scanned_from = match prior_fully_scanned_height {
            Some(h) => BlockHeight::from(h.as_u32().saturating_add(1)),
            None => self
                .inner
                .storage
                .wallet_birthday()
                .await?
                .unwrap_or_else(|| BlockHeight::from(1)),
        };
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: scanned_from,
            target_height: chain_tip,
        });

        if scanned_from.as_u32() > chain_tip.as_u32() {
            return Ok(self.emit_already_caught_up(scanned_from, chain_tip, reorg));
        }
        let blocks = fetch_compact_blocks(chain, scanned_from, chain_tip).await?;
        let block_count = u64::try_from(blocks.len()).unwrap_or(u64::MAX);
        if blocks.is_empty() {
            return Ok(self.emit_already_caught_up(scanned_from, chain_tip, reorg));
        }
        let from_state = fetch_prior_chain_state(chain, scanned_from).await?;
        self.scan_and_emit(
            ScanContext {
                blocks,
                scanned_from,
                target_height: chain_tip,
                block_count,
                reorgs_observed: reorg,
            },
            from_state,
        )
        .await
    }

    fn detect_tip_regress(
        &self,
        prior_observed_tip: Option<BlockHeight>,
        new_tip_height: BlockHeight,
    ) -> u32 {
        let Some(prior) = prior_observed_tip else {
            return 0;
        };
        if new_tip_height.as_u32() >= prior.as_u32() {
            return 0;
        }
        self.publish_event(WalletEvent::ReorgDetected {
            rolled_back_to_height: new_tip_height,
            new_tip_height,
        });
        1
    }

    fn emit_already_caught_up(
        &self,
        scanned_from: BlockHeight,
        target_height: BlockHeight,
        reorgs_observed: u32,
    ) -> SyncOutcome {
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: target_height,
            target_height,
        });
        SyncOutcome {
            scanned_from_height: scanned_from,
            scanned_to_height: target_height,
            block_count: 0,
            reorgs_observed,
        }
    }

    async fn scan_and_emit(
        &self,
        context: ScanContext,
        from_state: ChainState,
    ) -> Result<SyncOutcome, WalletError> {
        let ScanContext {
            blocks,
            scanned_from,
            target_height,
            block_count,
            reorgs_observed,
        } = context;
        let timestamps_by_height = block_timestamp_index(&blocks);
        let outcome = match self
            .inner
            .storage
            .scan_blocks(ScanRequest::new(blocks, scanned_from, from_state))
            .await
        {
            Ok(outcome) => outcome,
            Err(StorageError::ChainReorgDetected { at_height }) => {
                return self
                    .roll_back_after_reorg(at_height, target_height, reorgs_observed)
                    .await;
            }
            Err(other) => return Err(WalletError::from(other)),
        };

        let newly_confirmed = self
            .inner
            .storage
            .wallet_tx_ids_mined_in_range(scanned_from, outcome.scanned_to_height)
            .await?;
        for (tx_id, confirmed_at_height) in newly_confirmed {
            self.publish_event(WalletEvent::TransactionConfirmed {
                tx_id,
                confirmed_at_height,
            });
        }

        let received_notes = self
            .inner
            .storage
            .received_shielded_notes_mined_in_range(scanned_from, outcome.scanned_to_height)
            .await?;
        for note in received_notes {
            let block_timestamp_ms = if note.block_timestamp_ms != 0 {
                note.block_timestamp_ms
            } else {
                timestamps_by_height
                    .get(&note.mined_height.as_u32())
                    .copied()
                    .unwrap_or(0)
            };
            self.publish_event(WalletEvent::ShieldedReceiveObserved {
                account_id: note.account_id,
                tx_id: note.tx_id,
                output_index: note.output_index,
                value_zat: note.value_zat,
                mined_height: note.mined_height,
                block_timestamp_ms,
                pool: shielded_pool_for(note.protocol),
                is_change: note.is_change,
                spent_our_inputs: note.spent_our_inputs,
            });
        }

        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: outcome.scanned_to_height,
            target_height,
        });
        Ok(SyncOutcome {
            scanned_from_height: scanned_from,
            scanned_to_height: outcome.scanned_to_height,
            block_count,
            reorgs_observed,
        })
    }
}

impl Wallet {
    /// Recovers from a chain-source-side reorg observed during scan.
    ///
    /// `scan_cached_blocks` rejected the batch because the parent hash of the next block did
    /// not match the wallet's view at `at_height`. Truncate the wallet back by a full reorg
    /// window below the divergence (floored at the wallet birthday), emit `ReorgDetected`,
    /// and return a no-op outcome for this sync. The next sync iteration re-fetches from the
    /// new fully-scanned height, pulls fresh `ChainState` from the chain source, and resumes.
    ///
    /// Rolling back a window rather than a single block is what guarantees forward progress:
    /// `scan_cached_blocks` reports the divergence at `fully_scanned_height + 1`, so an
    /// `at_height - 1` rollback would leave the wallet exactly where it was and the next sync
    /// would re-attack the identical poisoned range forever.
    async fn roll_back_after_reorg(
        &self,
        at_height: BlockHeight,
        target_height: BlockHeight,
        prior_reorgs: u32,
    ) -> Result<SyncOutcome, WalletError> {
        let birthday = self
            .inner
            .storage
            .wallet_birthday()
            .await?
            .unwrap_or(BlockHeight::GENESIS);
        let rollback_to = reorg_rollback_target(at_height, birthday);
        let new_fully_scanned = self.inner.storage.truncate_to_height(rollback_to).await?;
        tracing::warn!(
            target: "zally::wallet::sync",
            event = "wallet_reorg_recovered",
            at_height = at_height.as_u32(),
            rolled_back_to_height = new_fully_scanned.as_u32(),
            target_height = target_height.as_u32(),
            "rolled wallet back after chain reorg; next sync will re-fetch"
        );
        self.publish_event(WalletEvent::ReorgDetected {
            rolled_back_to_height: new_fully_scanned,
            new_tip_height: target_height,
        });
        Ok(SyncOutcome {
            scanned_from_height: new_fully_scanned,
            scanned_to_height: new_fully_scanned,
            block_count: 0,
            reorgs_observed: prior_reorgs.saturating_add(1),
        })
    }
}

/// Computes the height to truncate the wallet to after `scan_cached_blocks` reports a
/// divergence at `at_height`.
///
/// Rolls back a full reorg window below the divergence so the next sync re-fetches a fresh
/// range and makes forward progress: `scan_cached_blocks` reports the divergence at
/// `fully_scanned_height + 1`, so a single-block rollback would leave the wallet exactly
/// where it was. Floored at the wallet birthday so a deep-height wallet never re-scans from
/// genesis.
fn reorg_rollback_target(at_height: BlockHeight, birthday: BlockHeight) -> BlockHeight {
    at_height
        .saturating_sub(REORG_ROLLBACK_DEPTH_BLOCKS)
        .max(birthday)
}

fn block_timestamp_index(
    blocks: &[zcash_client_backend::proto::compact_formats::CompactBlock],
) -> HashMap<u32, u64> {
    blocks
        .iter()
        .map(|block| {
            let height = u32::try_from(block.height).unwrap_or(u32::MAX);
            let timestamp_ms = u64::from(block.time).saturating_mul(1_000);
            (height, timestamp_ms)
        })
        .collect()
}

const fn shielded_pool_for(protocol: zcash_protocol::ShieldedProtocol) -> ShieldedPool {
    match protocol {
        zcash_protocol::ShieldedProtocol::Sapling => ShieldedPool::Sapling,
        zcash_protocol::ShieldedProtocol::Orchard => ShieldedPool::Orchard,
    }
}

async fn fetch_compact_blocks(
    chain: &dyn ChainSource,
    scanned_from: BlockHeight,
    target_height: BlockHeight,
) -> Result<Vec<zcash_client_backend::proto::compact_formats::CompactBlock>, WalletError> {
    let span_end = scanned_from
        .as_u32()
        .saturating_add(MAX_BLOCKS_PER_SYNC.saturating_sub(1))
        .min(target_height.as_u32());
    let range = BlockHeightRange {
        start_height: scanned_from,
        end_height: BlockHeight::from(span_end),
    };
    let mut stream = chain
        .compact_blocks(range)
        .await
        .map_err(|e| map_chain_source_error(&e))?;
    let mut blocks = Vec::new();
    while let Some(stream_item) = stream.next().await {
        let block = stream_item.map_err(|e| map_chain_source_error(&e))?;
        blocks.push(block);
    }
    Ok(blocks)
}

fn map_chain_source_error(err: &ChainSourceError) -> WalletError {
    WalletError::ChainSource {
        reason: err.to_string(),
        is_retryable: err.is_retryable(),
    }
}

async fn fetch_prior_chain_state(
    chain: &dyn ChainSource,
    scanned_from: BlockHeight,
) -> Result<ChainState, WalletError> {
    let prior_height = BlockHeight::from(scanned_from.as_u32().saturating_sub(1));
    let tree_state = chain
        .tree_state_at(prior_height)
        .await
        .map_err(|e| map_chain_source_error(&e))?;
    tree_state
        .to_chain_state()
        .map_err(|io| WalletError::ChainSource {
            reason: format!(
                "invalid tree state at height {}: {io}",
                prior_height.as_u32()
            ),
            is_retryable: false,
        })
}

#[cfg(test)]
mod tests {
    use super::{REORG_ROLLBACK_DEPTH_BLOCKS, reorg_rollback_target};
    use zally_core::BlockHeight;

    #[test]
    fn rollback_target_lands_a_window_below_a_boundary_divergence() {
        // The wedge: scan_cached_blocks reports the divergence at `fully_scanned_height + 1`.
        // The rollback must land strictly below `fully_scanned_height` so the next sync
        // re-fetches a fresh range instead of re-attacking the identical poisoned block.
        let fully_scanned_height = 4_009_770;
        let at_height = BlockHeight::from(fully_scanned_height + 1);
        let birthday = BlockHeight::from(4_009_000);
        let target = reorg_rollback_target(at_height, birthday);
        assert!(
            target.as_u32() < fully_scanned_height,
            "rollback target {} must be below fully_scanned_height {fully_scanned_height}",
            target.as_u32(),
        );
        assert_eq!(
            target.as_u32(),
            at_height.as_u32() - REORG_ROLLBACK_DEPTH_BLOCKS,
        );
    }

    #[test]
    fn rollback_target_is_floored_at_birthday() {
        // A divergence within the rollback window of the birthday must not roll back below it.
        let at_height = BlockHeight::from(4_009_010);
        let birthday = BlockHeight::from(4_009_000);
        assert_eq!(reorg_rollback_target(at_height, birthday), birthday);
    }

    #[test]
    fn rollback_target_saturates_to_birthday_floor() {
        // A low-height divergence saturates at the birthday rather than underflowing.
        let at_height = BlockHeight::from(50);
        let birthday = BlockHeight::from(1);
        assert_eq!(reorg_rollback_target(at_height, birthday), birthday);
    }
}
