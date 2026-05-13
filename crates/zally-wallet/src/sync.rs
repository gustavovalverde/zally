//! Wallet sync loop.
//!
//! Slice 5 wires `Wallet::sync` against `zcash_client_backend::data_api::chain::scan_cached_blocks`
//! via the storage-side `WalletStorage::scan_blocks` extension. The chain source streams
//! compact blocks; the wallet drains them, builds a `ChainState`, and hands both to the
//! storage layer which drives the upstream scanner against the live `WalletDb`.
//!
//! v1 invariant: each call re-scans from the wallet's last fully-scanned height up to the
//! current chain tip. Incremental sync with cross-call commitment-tree continuity is a v1
//! follow-up; the current implementation rebuilds the `ChainState` from the embedded
//! genesis frontier on every call, which is correct but linear-in-tip-height.

use futures_util::StreamExt as _;
use zally_chain::{BlockHeightRange, ChainSource, ChainSourceError, ChainState};
use zally_core::BlockHeight;
use zally_storage::ScanRequest;
use zcash_primitives::block::BlockHash;

use crate::event::WalletEvent;
use crate::retry::with_breaker_and_retry;
use crate::wallet::Wallet;
use crate::wallet_error::WalletError;

const MAX_BLOCKS_PER_SYNC: u32 = 1_000;

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
    /// Advances the wallet from its last-scanned height to `chain.chain_tip()`.
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
        let target_height = with_breaker_and_retry(
            &self.inner.circuit_breaker,
            policy,
            "sync.chain_tip",
            || chain.chain_tip(),
            |e| map_chain_source_error(&e),
        )
        .await?;
        let prior_observed_tip = self.inner.storage.lookup_observed_tip().await?;
        let reorg = self.detect_tip_regress(prior_observed_tip, target_height);
        self.inner
            .storage
            .record_observed_tip(target_height)
            .await?;

        let prior_fully_scanned_height = self.inner.storage.fully_scanned_height().await?;
        let scanned_from = prior_fully_scanned_height.map_or_else(
            || BlockHeight::from(1),
            |h| BlockHeight::from(h.as_u32().saturating_add(1)),
        );
        self.publish_event(WalletEvent::ScanProgress {
            scanned_height: scanned_from,
            target_height,
        });

        if scanned_from.as_u32() > target_height.as_u32() {
            return Ok(self.emit_already_caught_up(scanned_from, target_height, reorg));
        }
        let blocks = fetch_compact_blocks(chain, scanned_from, target_height).await?;
        let block_count = u64::try_from(blocks.len()).unwrap_or(u64::MAX);
        if blocks.is_empty() {
            return Ok(self.emit_already_caught_up(scanned_from, target_height, reorg));
        }
        self.scan_and_emit(ScanContext {
            blocks,
            scanned_from,
            target_height,
            block_count,
            reorgs_observed: reorg,
        })
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

    async fn scan_and_emit(&self, context: ScanContext) -> Result<SyncOutcome, WalletError> {
        let ScanContext {
            blocks,
            scanned_from,
            target_height,
            block_count,
            reorgs_observed,
        } = context;
        let from_state_height: zcash_protocol::consensus::BlockHeight =
            scanned_from.as_u32().saturating_sub(1).into();
        let from_state = ChainState::empty(from_state_height, BlockHash([0_u8; 32]));
        let outcome = self
            .inner
            .storage
            .scan_blocks(ScanRequest::new(blocks, scanned_from, from_state))
            .await?;

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
