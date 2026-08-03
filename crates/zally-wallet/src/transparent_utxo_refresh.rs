//! Transparent UTXO refresh, decoupled from block-scan cadence.
//!
//! `Wallet::refresh_transparent_utxos` pins one source [`zally_chain::ChainEpoch`] and the
//! wallet's scanned frontier, then walks every wallet-owned transparent receiver, recording
//! each receiver's UTXOs as soon as they are fetched. Every commit carries the pinned
//! frontier as a floor, so a reorg rewind on the block-scan loop that lands between one
//! receiver's fetch and its commit is rejected rather than written.
//! [`run_transparent_utxo_refresh_driver`] repeats that walk on its own cadence, independent
//! of the [`crate::SyncDriver`] block-scan loop: a receiver set whose refresh cost grows with
//! on-chain activity never occupies the scan attempt's timeout or retry ladder, and a slow or
//! faulted refresh never blocks `Wallet::sync` from reporting the wallet's block-scan
//! progress.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};
use zally_chain::ChainSource;
use zally_core::TxId;
use zally_storage::{TransparentReceiverRow, TransparentUtxoRow};

use crate::error::WalletError;
use crate::event::WalletEvent;
use crate::retry::with_breaker_and_retry;
use crate::wallet::{Wallet, current_unix_ms};

/// Summary of one [`Wallet::refresh_transparent_utxos`] attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct TransparentUtxoRefreshOutcome {
    /// Number of transparent receivers the walk read from the chain source.
    pub receivers_visited: u64,
    /// Number of transparent UTXOs recorded during this walk.
    pub utxo_count: u64,
    /// Unix milliseconds when this walk completed.
    pub completed_at_ms: u64,
}

impl Wallet {
    /// Refreshes every wallet-owned transparent receiver's UTXO set against `chain`.
    ///
    /// Pins one chain epoch for the whole walk: every receiver's read observes that epoch,
    /// and a rotation mid-walk surfaces as `ChainSourceError::ChainEpochPinUnavailable`
    /// ([`zally_chain::FailurePosture::Restartable`]), aborting the walk rather than mixing
    /// two epochs' results. Each receiver's UTXOs are recorded as soon as they are fetched,
    /// so an aborted walk keeps every receiver it already visited; a retry (or the next
    /// scheduled walk) revisits every receiver under a fresh pin, which is safe because
    /// `WalletStorage::record_transparent_utxos` is idempotent.
    ///
    /// Also pins the wallet's scanned frontier at the same moment: every receiver's commit
    /// carries that floor, so a reorg rewind landing on the block-scan loop between one
    /// receiver's fetch and its commit surfaces as `StorageError::ScanFrontierReceded`
    /// (`Restartable`) instead of writing UTXOs against blocks the wallet no longer attests.
    ///
    /// `requires_operator` on network mismatch. `retryable` on transient chain-source
    /// failures. `restartable` on a rotated epoch pin or a receded scan frontier.
    pub async fn refresh_transparent_utxos(
        &self,
        chain: &dyn ChainSource,
    ) -> Result<TransparentUtxoRefreshOutcome, WalletError> {
        with_breaker_and_retry(
            &self.inner.circuit_breaker,
            self.retry_policy(),
            "sync.transparent_utxo_refresh",
            || self.refresh_transparent_utxos_inner(chain),
            std::convert::identity,
        )
        .await
    }

    async fn refresh_transparent_utxos_inner(
        &self,
        chain: &dyn ChainSource,
    ) -> Result<TransparentUtxoRefreshOutcome, WalletError> {
        let chain_epoch = self.pin_chain_epoch(chain).await?;
        let min_scanned_height = self.inner.storage.fully_scanned_height().await?;
        let receivers = self.inner.storage.list_transparent_receivers().await?;
        let receiver_count = u64::try_from(receivers.len()).unwrap_or(u64::MAX);
        let started_at_ms = current_unix_ms();
        tracing::info!(
            target: "zally::sync",
            event = "wallet_transparent_utxo_refresh_started",
            receivers = receiver_count,
            "starting transparent UTXO refresh walk"
        );

        let mut seen_outpoints = HashSet::new();
        let mut utxo_count: u64 = 0;
        for (index, receiver) in receivers.into_iter().enumerate() {
            let receiver_index = u64::try_from(index).unwrap_or(u64::MAX);
            let utxos = match chain
                .transparent_utxos(chain_epoch, &receiver.script_pub_key_bytes)
                .await
            {
                Ok(utxos) => utxos,
                Err(source_error) => {
                    tracing::warn!(
                        target: "zally::sync",
                        event = "wallet_transparent_utxo_refresh_faulted",
                        receiver_index,
                        receivers = receiver_count,
                        posture = source_error.posture().label(),
                        reason = %source_error,
                        "transparent UTXO refresh walk aborted"
                    );
                    return Err(WalletError::ChainSource(source_error));
                }
            };
            let rows = validate_transparent_utxo_batch(
                chain_epoch,
                &receiver,
                utxos,
                &mut seen_outpoints,
            )?;
            let recorded = self
                .inner
                .storage
                .record_transparent_utxos(min_scanned_height, rows)
                .await?;
            utxo_count = utxo_count.saturating_add(recorded);
            tracing::debug!(
                target: "zally::sync",
                event = "wallet_transparent_utxo_receiver_completed",
                receiver_index,
                receivers = receiver_count,
                utxo_count = recorded,
                "refreshed one transparent receiver"
            );
        }

        let outcome = TransparentUtxoRefreshOutcome {
            receivers_visited: receiver_count,
            utxo_count,
            completed_at_ms: current_unix_ms(),
        };
        tracing::info!(
            target: "zally::sync",
            event = "wallet_transparent_utxo_refresh_completed",
            receivers = receiver_count,
            utxo_count,
            elapsed_ms = outcome.completed_at_ms.saturating_sub(started_at_ms),
            "transparent UTXO refresh walk completed"
        );
        Ok(outcome)
    }
}

/// Long-lived loop that refreshes transparent UTXOs on its own cadence.
///
/// Runs independently of the block-scan [`crate::SyncDriver`] loop: a failed attempt logs
/// and waits for the next tick rather than engaging a repair ladder, since a stale
/// transparent-UTXO cache is neither a corrupt wallet state nor a reason to stop scanning
/// blocks. Waits one interval before its first attempt, so a freshly started driver's block
/// scan gets the chain source to itself rather than racing this loop's first request.
///
/// Bounds every attempt to `timeout_seconds`, so a chain read that never returns faults the
/// attempt instead of wedging the loop forever. Every faulted attempt (including a timed-out
/// one) publishes [`WalletEvent::TransparentUtxoRefreshFaulted`] carrying the running
/// consecutive-failure count, so a host observes a stall through the wallet's event stream
/// rather than only in logs.
pub(crate) async fn run_transparent_utxo_refresh_driver(
    wallet: Wallet,
    chain: Arc<dyn ChainSource>,
    interval_ms: u64,
    timeout_seconds: u64,
    mut close_rx: oneshot::Receiver<()>,
) {
    let mut consecutive_failures: u32 = 0;
    loop {
        tokio::select! {
            biased;
            _ = &mut close_rx => return,
            () = sleep(Duration::from_millis(interval_ms)) => {}
        }
        let attempt = tokio::select! {
            biased;
            _ = &mut close_rx => return,
            attempt = timeout(
                Duration::from_secs(timeout_seconds),
                wallet.refresh_transparent_utxos(chain.as_ref()),
            ) => attempt,
        };
        let fault_reason = match attempt {
            Ok(Ok(_outcome)) => {
                consecutive_failures = 0;
                None
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    target: "zally::sync",
                    event = "wallet_transparent_utxo_refresh_cycle_failed",
                    posture = error.posture().label(),
                    reason = %error,
                    "transparent UTXO refresh exhausted its retries"
                );
                Some(error.to_string())
            }
            Err(_elapsed) => {
                let reason = format!("transparent UTXO refresh exceeded {timeout_seconds} seconds");
                tracing::warn!(
                    target: "zally::sync",
                    event = "wallet_transparent_utxo_refresh_cycle_timed_out",
                    reason = %reason,
                    "transparent UTXO refresh attempt timed out"
                );
                Some(reason)
            }
        };
        if let Some(reason) = fault_reason {
            consecutive_failures = consecutive_failures.saturating_add(1);
            wallet.publish_event(WalletEvent::TransparentUtxoRefreshFaulted {
                consecutive_failures,
                reason,
            });
        }
    }
}

/// Validates one receiver's chain-reported UTXOs against the pinned epoch's visible tip, the
/// receiver's own script, and every outpoint seen so far in this walk, returning the rows
/// ready to record.
///
/// `seen_outpoints` accumulates across the whole walk (not just this receiver) so a chain
/// source that reports the same outpoint under two receivers is caught even though each
/// receiver commits independently.
fn validate_transparent_utxo_batch(
    chain_epoch: zally_chain::ChainEpoch,
    receiver: &TransparentReceiverRow,
    utxos: Vec<zally_chain::TransparentUtxo>,
    seen_outpoints: &mut HashSet<(TxId, u32)>,
) -> Result<Vec<TransparentUtxoRow>, WalletError> {
    let mut rows = Vec::with_capacity(utxos.len());
    for utxo in utxos {
        if utxo.confirmed_at_height > chain_epoch.visible_tip().height {
            return Err(WalletError::ChainSource(
                zally_chain::ChainSourceError::MalformedTransparentUtxoSet {
                    reason: format!(
                        "outpoint {}:{} is confirmed at {} above pinned visible tip {}",
                        utxo.tx_id,
                        utxo.output_index,
                        utxo.confirmed_at_height,
                        chain_epoch.visible_tip().height,
                    ),
                },
            ));
        }
        if !seen_outpoints.insert((utxo.tx_id, utxo.output_index)) {
            return Err(WalletError::ChainSource(
                zally_chain::ChainSourceError::MalformedTransparentUtxoSet {
                    reason: format!(
                        "outpoint {}:{} appears more than once",
                        utxo.tx_id, utxo.output_index
                    ),
                },
            ));
        }
        if utxo.script_pub_key_bytes != receiver.script_pub_key_bytes {
            return Err(WalletError::ChainSource(
                zally_chain::ChainSourceError::MalformedTransparentUtxoSet {
                    reason: format!(
                        "outpoint {}:{} script did not match wallet receiver for account {:?}",
                        utxo.tx_id, utxo.output_index, receiver.account_id
                    ),
                },
            ));
        }
        rows.push(TransparentUtxoRow::new(
            utxo.tx_id,
            utxo.output_index,
            utxo.value_zat,
            utxo.confirmed_at_height,
            utxo.script_pub_key_bytes,
        ));
    }
    Ok(rows)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit-test constructors use fixed values whose invalidity is a fixture bug"
)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use zally_chain::{
        BlockHeightRange, BlockId, ChainEpoch, ChainEpochId, ChainEventEnvelopeStream,
        ChainEventStreamStart, ChainSourceError, CompactBlockStream, ShieldedPool, SubtreeIndex,
        SubtreeRoot, TransactionStatus, TransparentUtxo,
    };
    use zally_core::{BlockHash, BlockHeight, Network, TreeStateArtifact};
    use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
    use zally_storage::{Sqlite, SqliteOptions, StorageError};
    use zally_testkit::{MockChainSource, TempWalletPath};

    use super::*;
    use crate::retry::RetryPolicy;

    fn transparent_test_epoch() -> ChainEpoch {
        let tip = BlockId {
            height: BlockHeight::from(10),
            hash: BlockHash::from_bytes([10; 32]),
        };
        ChainEpoch::new(ChainEpochId::new(1), Network::regtest(), tip, tip)
            .expect("test epoch is valid")
    }

    fn transparent_receiver(id: u128, script: &[u8]) -> TransparentReceiverRow {
        TransparentReceiverRow::new(
            zally_core::AccountId::from_uuid(uuid::Uuid::from_u128(id)),
            script.to_vec(),
        )
    }

    fn transparent_utxo(tx_byte: u8, script: &[u8], height: u32) -> TransparentUtxo {
        TransparentUtxo {
            tx_id: TxId::from_bytes([tx_byte; 32]),
            output_index: 0,
            value_zat: zally_core::Zatoshis::try_from(1).unwrap_or(zally_core::Zatoshis::zero()),
            confirmed_at_height: BlockHeight::from(height),
            script_pub_key_bytes: script.to_vec(),
        }
    }

    #[test]
    fn malformed_receiver_yields_no_storage_batch() {
        let mut seen = HashSet::new();
        let receiver = transparent_receiver(1, &[1]);
        let result = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &receiver,
            vec![transparent_utxo(1, &[1], 11)],
            &mut seen,
        );
        assert!(matches!(
            result,
            Err(WalletError::ChainSource(
                ChainSourceError::MalformedTransparentUtxoSet { .. }
            ))
        ));
    }

    #[test]
    fn cross_receiver_duplicate_yields_no_storage_batch() {
        let mut seen = HashSet::new();
        let duplicate = transparent_utxo(1, &[1], 9);
        let first_receiver = transparent_receiver(1, &[1]);
        let first = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &first_receiver,
            vec![duplicate.clone()],
            &mut seen,
        );
        assert!(first.is_ok(), "the first receiver's read is well-formed");

        let second_receiver = transparent_receiver(2, &[1]);
        let second = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &second_receiver,
            vec![duplicate],
            &mut seen,
        );
        assert!(matches!(
            second,
            Err(WalletError::ChainSource(
                ChainSourceError::MalformedTransparentUtxoSet { .. }
            ))
        ));
    }

    #[test]
    fn script_mismatch_yields_no_storage_batch() {
        let mut seen = HashSet::new();
        let receiver = transparent_receiver(1, &[1]);
        let result = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &receiver,
            vec![transparent_utxo(1, &[2], 9)],
            &mut seen,
        );
        assert!(matches!(
            result,
            Err(WalletError::ChainSource(
                ChainSourceError::MalformedTransparentUtxoSet { .. }
            ))
        ));
    }

    #[test]
    fn well_formed_batch_yields_one_row_per_utxo() {
        let mut seen = HashSet::new();
        let receiver = transparent_receiver(1, &[1]);
        let rows = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &receiver,
            vec![transparent_utxo(1, &[1], 9), transparent_utxo(2, &[1], 10)],
            &mut seen,
        )
        .expect("well-formed batch validates");
        assert_eq!(rows.len(), 2);
    }

    /// Regression: a reorg rewind landing between one receiver's chain read and its commit
    /// must reject the commit rather than write UTXOs against blocks the wallet no longer
    /// attests.
    #[tokio::test]
    async fn reorg_rewind_mid_walk_prevents_the_commit() {
        let temp = TempWalletPath::create().expect("temp wallet path");
        let network = Network::regtest();
        let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
        let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
        let bootstrap_chain = MockChainSource::new(network);
        let (wallet, _account_id, _mnemonic) = Wallet::builder(network, sealing, storage)
            .create(&bootstrap_chain, BlockHeight::from(1))
            .await
            .expect("wallet creation");

        let tip = BlockHeight::from(50);
        let bootstrap_handle = bootstrap_chain.handle();
        bootstrap_handle.serve_compact_blocks();
        bootstrap_handle.advance_tip(tip);
        let sync_outcome = wallet.sync(&bootstrap_chain).await.expect("initial sync");
        assert_eq!(sync_outcome.scanned_to_height, tip);

        wallet.set_retry_policy(RetryPolicy::none());
        let rewind_to = BlockHeight::from(10);
        let chain = RewindOnFirstTransparentFetch {
            inner: bootstrap_chain,
            wallet: wallet.clone(),
            rewind_to,
            triggered: AtomicBool::new(false),
        };

        let result = wallet.refresh_transparent_utxos(&chain).await;
        assert!(
            matches!(
                result,
                Err(WalletError::Storage(
                    StorageError::ScanFrontierReceded { .. }
                ))
            ),
            "a reorg rewind landing mid-walk must reject the commit, got {result:?}"
        );

        let status = wallet.status_snapshot().await.expect("status after rewind");
        assert_eq!(
            status.scanned_height,
            Some(rewind_to),
            "the rewind must have actually landed"
        );
    }

    /// `ChainSource` that delegates to an inner [`MockChainSource`], except that its first
    /// `transparent_utxos` call rewinds `wallet` to `rewind_to` before delegating.
    ///
    /// Stands in for the block-scan loop's repair ladder detecting a reorg and truncating the
    /// wallet's derived state while the transparent-UTXO refresh loop is mid-walk under an
    /// already-pinned epoch and scan frontier.
    struct RewindOnFirstTransparentFetch {
        inner: MockChainSource,
        wallet: Wallet,
        rewind_to: BlockHeight,
        triggered: AtomicBool,
    }

    #[async_trait]
    impl ChainSource for RewindOnFirstTransparentFetch {
        fn network(&self) -> Network {
            self.inner.network()
        }

        async fn current_epoch(&self) -> Result<ChainEpoch, ChainSourceError> {
            self.inner.current_epoch().await
        }

        async fn compact_blocks(
            &self,
            chain_epoch: ChainEpoch,
            block_range: BlockHeightRange,
        ) -> Result<CompactBlockStream, ChainSourceError> {
            self.inner.compact_blocks(chain_epoch, block_range).await
        }

        async fn tree_state_at(
            &self,
            chain_epoch: ChainEpoch,
            block_height: BlockHeight,
        ) -> Result<TreeStateArtifact, ChainSourceError> {
            self.inner.tree_state_at(chain_epoch, block_height).await
        }

        async fn subtree_roots(
            &self,
            chain_epoch: ChainEpoch,
            pool: ShieldedPool,
            start_index: SubtreeIndex,
            max_count: u32,
        ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
            self.inner
                .subtree_roots(chain_epoch, pool, start_index, max_count)
                .await
        }

        async fn transaction_status(
            &self,
            tx_id: TxId,
        ) -> Result<TransactionStatus, ChainSourceError> {
            self.inner.transaction_status(tx_id).await
        }

        async fn transparent_utxos(
            &self,
            chain_epoch: ChainEpoch,
            script_pub_key_bytes: &[u8],
        ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
            if !self.triggered.swap(true, Ordering::SeqCst) {
                self.wallet
                    .rewind_to_height(&self.inner, self.rewind_to)
                    .await
                    .expect("mid-walk rewind must succeed");
            }
            self.inner
                .transparent_utxos(chain_epoch, script_pub_key_bytes)
                .await
        }

        async fn chain_event_envelopes(
            &self,
            start: ChainEventStreamStart,
        ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
            self.inner.chain_event_envelopes(start).await
        }
    }
}
