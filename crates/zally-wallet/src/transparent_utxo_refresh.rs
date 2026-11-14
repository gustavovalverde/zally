//! Transparent UTXO refresh, decoupled from block-scan cadence.
//!
//! `Wallet::refresh_transparent_utxos` walks every wallet-owned transparent receiver,
//! reconciling each receiver's recorded UTXOs with the complete unspent set the chain source
//! reports as soon as it is fetched: new outputs are recorded and outputs the set no longer
//! contains are retired. Each attempt pins one source [`zally_chain::ChainEpoch`] and the
//! wallet's scanned frontier; every commit carries that frontier as a floor, so a reorg rewind
//! on the block-scan loop that lands between one receiver's fetch and its commit is rejected
//! rather than written. A retryable or restartable failure re-pins both and resumes at the
//! receiver that failed, under the wallet's transparent-UTXO refresh retry policy.
//! [`run_transparent_utxo_refresh_driver`] repeats that walk on its own cadence, independent
//! of the [`crate::SyncDriver`] block-scan loop: a receiver set whose refresh cost grows with
//! on-chain activity never occupies the scan attempt's timeout or retry ladder, and a slow or
//! faulted refresh never blocks `Wallet::sync` from reporting the wallet's block-scan
//! progress.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};
use zally_chain::ChainSource;
use zally_core::TxId;
use zally_storage::{TransparentReceiverRow, TransparentUnspentSet, TransparentUtxoRow};

use crate::error::WalletError;
use crate::event::WalletEvent;
use crate::retry::{RetryPolicy, with_breaker_and_retry};
use crate::wallet::{Wallet, current_unix_ms};

/// Default [`Wallet::transparent_utxo_refresh_retry_policy`]: 8 attempts, backing off from
/// 250 milliseconds doubling to an 8-second cap (about 24 seconds of backoff in all).
pub(crate) const DEFAULT_TRANSPARENT_UTXO_REFRESH_RETRY_POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 8,
    initial_backoff_ms: 250,
    max_backoff_ms: 8_000,
    backoff_multiplier_x10: 20,
};

/// Summary of one completed [`Wallet::refresh_transparent_utxos`] walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct TransparentUtxoRefreshOutcome {
    /// Number of transparent receivers the walk read from the chain source.
    pub receivers_visited: u64,
    /// Number of transparent UTXOs recorded during this walk.
    pub utxo_count: u64,
    /// Number of recorded transparent UTXOs this walk retired because the chain source no
    /// longer reports them.
    pub utxos_retired: u64,
    /// Unix milliseconds when this walk completed.
    pub completed_at_ms: u64,
}

/// Progress of one walk, carried across its retry attempts.
#[derive(Default)]
struct TransparentUtxoWalk {
    attempts: u32,
    receivers: Option<Arc<[TransparentReceiverRow]>>,
    receivers_refreshed: usize,
    recorded_outpoints: HashSet<(TxId, u32)>,
    utxo_count: u64,
    utxos_retired: u64,
    started_at_ms: u64,
}

impl TransparentUtxoWalk {
    fn incomplete(&self, source: WalletError) -> WalletError {
        WalletError::TransparentUtxoRefreshIncomplete {
            receivers_refreshed: u64::try_from(self.receivers_refreshed).unwrap_or(u64::MAX),
            receivers: self.receivers.as_ref().map_or(0, |receivers| {
                u64::try_from(receivers.len()).unwrap_or(u64::MAX)
            }),
            attempts: self.attempts,
            source: Box::new(source),
        }
    }
}

impl Wallet {
    /// Refreshes every wallet-owned transparent receiver's UTXO set against `chain`.
    ///
    /// Each attempt pins one chain epoch and the wallet's scanned frontier, and every receiver
    /// read and commit in that attempt uses them. A failure whose posture allows a retry (a
    /// source that is not ready, a rotated epoch pin, a receded scan frontier) backs off under
    /// [`Self::transparent_utxo_refresh_retry_policy`], re-pins both, and resumes at the
    /// receiver that failed. Receivers already refreshed keep their reconciled UTXOs, so a
    /// source that rotates its epoch faster than one whole walk still completes it. Mixing
    /// epochs across receivers is safe: each receiver's set is validated against, and
    /// reconciled with, the epoch that served it, so no receiver's result depends on
    /// another's epoch.
    ///
    /// Each receiver's set is complete, so `WalletStorage::record_transparent_unspent_set`
    /// also retires a recorded output the set omits: one mined at or below the epoch's
    /// visible tip, that no mined or unexpired transaction spends, from a transaction that
    /// carries nothing else of the wallet's. A reorg that replaced the output's block leaves
    /// exactly that, even below the wallet's scanned height, and shielding it would only get
    /// the transaction rejected for a missing input. A retired output leaves every balance and
    /// proposal; a later walk that sees it again records it again.
    ///
    /// The scanned-frontier floor means a reorg rewind landing on the block-scan loop between
    /// one receiver's fetch and its commit surfaces as `StorageError::ScanFrontierReceded`
    /// (`Restartable`) instead of writing UTXOs against blocks the wallet no longer attests.
    ///
    /// # Errors
    ///
    /// Returns [`WalletError::CircuitBroken`] when the circuit breaker is open, and otherwise
    /// [`WalletError::TransparentUtxoRefreshIncomplete`] for a walk that stopped early. Its
    /// posture is the posture of the failure that ended the walk: `requires_operator` on
    /// network mismatch or a malformed UTXO set, `retryable` or `restartable` once the retry
    /// policy ran out of attempts.
    pub async fn refresh_transparent_utxos(
        &self,
        chain: &dyn ChainSource,
    ) -> Result<TransparentUtxoRefreshOutcome, WalletError> {
        let walk = Mutex::new(TransparentUtxoWalk::default());
        with_breaker_and_retry(
            &self.inner.circuit_breaker,
            self.transparent_utxo_refresh_retry_policy(),
            "sync.transparent_utxo_refresh",
            || self.resume_transparent_utxo_walk(chain, &walk),
            |error| walk.lock().incomplete(error),
        )
        .await
    }

    async fn resume_transparent_utxo_walk(
        &self,
        chain: &dyn ChainSource,
        walk: &Mutex<TransparentUtxoWalk>,
    ) -> Result<TransparentUtxoRefreshOutcome, WalletError> {
        let (attempt, receivers) = self.begin_transparent_utxo_attempt(walk).await?;
        let pinned = TransparentUtxoAttempt {
            attempt,
            receiver_count: u64::try_from(receivers.len()).unwrap_or(u64::MAX),
            chain_epoch: self.pin_chain_epoch(chain).await?,
            min_scanned_height: self.inner.storage.fully_scanned_height().await?,
        };
        let resume_index = walk.lock().receivers_refreshed;
        tracing::info!(
            target: "zally::sync",
            event = "wallet_transparent_utxo_refresh_started",
            receivers = pinned.receiver_count,
            receiver_index = u64::try_from(resume_index).unwrap_or(u64::MAX),
            attempt,
            "starting transparent UTXO refresh walk"
        );
        for (index, receiver) in receivers.iter().enumerate().skip(resume_index) {
            self.refresh_transparent_receiver(chain, &pinned, (index, receiver), walk)
                .await?;
        }

        let (utxo_count, utxos_retired, started_at_ms) = {
            let progress = walk.lock();
            (
                progress.utxo_count,
                progress.utxos_retired,
                progress.started_at_ms,
            )
        };
        let outcome = TransparentUtxoRefreshOutcome {
            receivers_visited: pinned.receiver_count,
            utxo_count,
            utxos_retired,
            completed_at_ms: current_unix_ms(),
        };
        tracing::info!(
            target: "zally::sync",
            event = "wallet_transparent_utxo_refresh_completed",
            receivers = pinned.receiver_count,
            utxo_count,
            utxos_retired,
            attempts = attempt,
            elapsed_ms = outcome.completed_at_ms.saturating_sub(started_at_ms),
            "transparent UTXO refresh walk completed"
        );
        Ok(outcome)
    }

    /// Counts one attempt and returns the walk's receivers, listing them on the first attempt
    /// that gets that far so every retry resumes over the same list.
    async fn begin_transparent_utxo_attempt(
        &self,
        walk: &Mutex<TransparentUtxoWalk>,
    ) -> Result<(u32, Arc<[TransparentReceiverRow]>), WalletError> {
        let (attempt, listed_receivers) = {
            let mut progress = walk.lock();
            progress.attempts = progress.attempts.saturating_add(1);
            (progress.attempts, progress.receivers.clone())
        };
        if let Some(receivers) = listed_receivers {
            return Ok((attempt, receivers));
        }
        let receivers: Arc<[TransparentReceiverRow]> = self
            .inner
            .storage
            .list_transparent_receivers()
            .await?
            .into();
        {
            let mut progress = walk.lock();
            progress.receivers = Some(Arc::clone(&receivers));
            progress.started_at_ms = current_unix_ms();
        }
        Ok((attempt, receivers))
    }

    async fn refresh_transparent_receiver(
        &self,
        chain: &dyn ChainSource,
        pinned: &TransparentUtxoAttempt,
        (index, receiver): (usize, &TransparentReceiverRow),
        walk: &Mutex<TransparentUtxoWalk>,
    ) -> Result<(), WalletError> {
        let receiver_index = u64::try_from(index).unwrap_or(u64::MAX);
        let utxos = match chain
            .transparent_utxos(pinned.chain_epoch, &receiver.script_pub_key_bytes)
            .await
        {
            Ok(utxos) => utxos,
            Err(source_error) => {
                tracing::warn!(
                    target: "zally::sync",
                    event = "wallet_transparent_utxo_refresh_faulted",
                    receiver_index,
                    receivers = pinned.receiver_count,
                    attempt = pinned.attempt,
                    posture = source_error.posture().label(),
                    reason = %source_error,
                    "transparent UTXO refresh walk interrupted"
                );
                return Err(WalletError::ChainSource(source_error));
            }
        };
        let rows = validate_transparent_utxo_batch(
            pinned.chain_epoch,
            receiver,
            utxos,
            &walk.lock().recorded_outpoints,
        )?;
        let outpoints: Vec<(TxId, u32)> = rows
            .iter()
            .map(|row| (row.tx_id, row.output_index))
            .collect();
        let recorded = self
            .inner
            .storage
            .record_transparent_unspent_set(
                pinned.min_scanned_height,
                TransparentUnspentSet::new(
                    pinned.chain_epoch.network(),
                    receiver.script_pub_key_bytes.clone(),
                    pinned.chain_epoch.visible_tip().height,
                    rows,
                ),
            )
            .await?;
        for outpoint in &recorded.retired_outpoints {
            tracing::warn!(
                target: "zally::sync",
                event = "wallet_transparent_utxo_retired",
                tx_id = %outpoint.tx_id,
                output_index = outpoint.output_index,
                receiver_index,
                observed_at_height = pinned.chain_epoch.visible_tip().height.as_u32(),
                "the chain source no longer reports a recorded transparent UTXO; it no longer \
                 counts toward balances or proposals"
            );
        }
        let retired_count = u64::try_from(recorded.retired_outpoints.len()).unwrap_or(u64::MAX);
        {
            let mut progress = walk.lock();
            progress.recorded_outpoints.extend(outpoints);
            progress.utxo_count = progress.utxo_count.saturating_add(recorded.recorded_count);
            progress.utxos_retired = progress.utxos_retired.saturating_add(retired_count);
            progress.receivers_refreshed = index.saturating_add(1);
        }
        tracing::debug!(
            target: "zally::sync",
            event = "wallet_transparent_utxo_receiver_completed",
            receiver_index,
            receivers = pinned.receiver_count,
            utxo_count = recorded.recorded_count,
            utxos_retired = retired_count,
            "refreshed one transparent receiver"
        );
        Ok(())
    }
}

/// Source boundaries one walk attempt pinned, shared by every receiver it refreshes.
struct TransparentUtxoAttempt {
    attempt: u32,
    receiver_count: u64,
    chain_epoch: zally_chain::ChainEpoch,
    min_scanned_height: Option<zally_core::BlockHeight>,
}

/// Long-lived loop that refreshes transparent UTXOs on its own cadence.
///
/// Runs independently of the block-scan [`crate::SyncDriver`] loop: a failed attempt logs
/// and waits for the next tick rather than engaging a repair ladder, since a stale
/// transparent-UTXO cache is neither a corrupt wallet state nor a reason to stop scanning
/// blocks. Waits one interval before its first attempt, so a freshly started driver's block
/// scan gets the chain source to itself rather than racing this loop's first request.
///
/// Each tick runs one [`Wallet::refresh_transparent_utxos`] call, which retries within the
/// tick under the wallet's transparent-UTXO refresh retry policy. Bounds every call to
/// `timeout_seconds`, retries included, so a chain read that never returns faults the tick
/// instead of wedging the loop forever. Every faulted tick (including a timed-out one)
/// publishes [`WalletEvent::TransparentUtxoRefreshFaulted`] carrying the running
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
/// receiver's own script, and every outpoint recorded so far in this walk, returning the rows
/// ready to record.
///
/// `recorded_outpoints` spans the whole walk (not just this receiver) so a chain source that
/// reports the same outpoint under two receivers is caught even though each receiver commits
/// independently. It holds only committed receivers' outpoints, so a receiver re-read after a
/// failed commit is not mistaken for its own duplicate.
fn validate_transparent_utxo_batch(
    chain_epoch: zally_chain::ChainEpoch,
    receiver: &TransparentReceiverRow,
    utxos: Vec<zally_chain::TransparentUtxo>,
    recorded_outpoints: &HashSet<(TxId, u32)>,
) -> Result<Vec<TransparentUtxoRow>, WalletError> {
    let mut rows = Vec::with_capacity(utxos.len());
    let mut batch_outpoints = HashSet::with_capacity(utxos.len());
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
        let outpoint = (utxo.tx_id, utxo.output_index);
        if recorded_outpoints.contains(&outpoint) || !batch_outpoints.insert(outpoint) {
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
        let recorded = HashSet::new();
        let receiver = transparent_receiver(1, &[1]);
        let result = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &receiver,
            vec![transparent_utxo(1, &[1], 11)],
            &recorded,
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
        let mut recorded = HashSet::new();
        let duplicate = transparent_utxo(1, &[1], 9);
        let first_receiver = transparent_receiver(1, &[1]);
        let first = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &first_receiver,
            vec![duplicate.clone()],
            &recorded,
        )
        .expect("the first receiver's read is well-formed");
        recorded.extend(first.iter().map(|row| (row.tx_id, row.output_index)));

        let second_receiver = transparent_receiver(2, &[1]);
        let second = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &second_receiver,
            vec![duplicate],
            &recorded,
        );
        assert!(matches!(
            second,
            Err(WalletError::ChainSource(
                ChainSourceError::MalformedTransparentUtxoSet { .. }
            ))
        ));
    }

    #[test]
    fn in_batch_duplicate_yields_no_storage_batch() {
        let recorded = HashSet::new();
        let receiver = transparent_receiver(1, &[1]);
        let result = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &receiver,
            vec![transparent_utxo(1, &[1], 9), transparent_utxo(1, &[1], 9)],
            &recorded,
        );
        assert!(matches!(
            result,
            Err(WalletError::ChainSource(
                ChainSourceError::MalformedTransparentUtxoSet { .. }
            ))
        ));
    }

    #[test]
    fn uncommitted_batch_validates_again_on_a_retry() {
        let recorded = HashSet::new();
        let receiver = transparent_receiver(1, &[1]);
        for _ in 0..2_u32 {
            let rows = validate_transparent_utxo_batch(
                transparent_test_epoch(),
                &receiver,
                vec![transparent_utxo(1, &[1], 9)],
                &recorded,
            )
            .expect("a batch whose commit failed must validate again when re-read");
            assert_eq!(rows.len(), 1);
        }
    }

    #[test]
    fn script_mismatch_yields_no_storage_batch() {
        let recorded = HashSet::new();
        let receiver = transparent_receiver(1, &[1]);
        let result = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &receiver,
            vec![transparent_utxo(1, &[2], 9)],
            &recorded,
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
        let recorded = HashSet::new();
        let receiver = transparent_receiver(1, &[1]);
        let rows = validate_transparent_utxo_batch(
            transparent_test_epoch(),
            &receiver,
            vec![transparent_utxo(1, &[1], 9), transparent_utxo(2, &[1], 10)],
            &recorded,
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

        wallet.set_transparent_utxo_refresh_retry_policy(RetryPolicy::none());
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
                &result,
                Err(WalletError::TransparentUtxoRefreshIncomplete {
                    receivers_refreshed: 0,
                    attempts: 1,
                    source,
                    ..
                }) if matches!(
                    source.as_ref(),
                    WalletError::Storage(StorageError::ScanFrontierReceded { .. })
                )
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
            chain_epoch: ChainEpoch,
            tx_id: TxId,
        ) -> Result<TransactionStatus, ChainSourceError> {
            self.inner.transaction_status(chain_epoch, tx_id).await
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

        async fn reorg_window_blocks(&self) -> Result<u32, ChainSourceError> {
            self.inner.reorg_window_blocks().await
        }
    }
}
