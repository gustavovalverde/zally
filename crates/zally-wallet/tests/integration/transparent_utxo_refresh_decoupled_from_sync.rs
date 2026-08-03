//! Regression: `Wallet::sync`'s wall-clock cost must not scale with the transparent-UTXO
//! walk's cost.
//!
//! The transparent-UTXO refresh runs on its own cadence; a slow or numerous-receiver walk
//! must not slow a scan attempt. This proves the two are decoupled by injecting real
//! latency into `transparent_utxos` and showing `Wallet::sync`
//! finishes almost immediately regardless, while `Wallet::refresh_transparent_utxos` still
//! pays the latency in full when called directly (proving the latency is real, not an
//! artifact of an empty receiver list).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use zally_chain::{
    BlockHeightRange, ChainEpoch, ChainEventEnvelopeStream, ChainEventStreamStart, ChainSource,
    ChainSourceError, CompactBlockStream, ShieldedPool, SubtreeIndex, SubtreeRoot,
    TransactionStatus, TransparentUtxo,
};
use zally_core::{BlockHeight, Network, TreeStateArtifact, TxId};
use zally_testkit::MockChainSource;
use zally_wallet::WalletError;

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

/// Sleep injected into every `transparent_utxos` call.
///
/// A fresh test wallet already carries several dozen pre-generated transparent receivers
/// (external, internal, and ephemeral scopes), so a small per-call latency is enough to add
/// up to a wall-clock cost that would be very obvious inline in `wallet.sync`, without making
/// this test slow.
const PER_CALL_LATENCY: Duration = Duration::from_millis(30);

/// Comfortably above what `wallet.sync` should ever take for one chunk on an in-memory mock;
/// comfortably below what the walk takes once it visits more than a handful of receivers.
const SYNC_WALL_CLOCK_CEILING: Duration = Duration::from_millis(400);

#[tokio::test]
async fn sync_wall_clock_is_independent_of_transparent_utxo_latency() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let inner = MockChainSource::new(network);
    let chain_handle = inner.handle();
    chain_handle.serve_compact_blocks();
    chain_handle.advance_tip(BlockHeight::from(50));

    let transparent_utxos_calls = Arc::new(AtomicUsize::new(0));
    let chain = LatentTransparentUtxoChainSource {
        inner,
        latency: PER_CALL_LATENCY,
        transparent_utxos_calls: Arc::clone(&transparent_utxos_calls),
    };

    let started = Instant::now();
    let outcome = wallet.sync(&chain).await?;
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.block_count, 50,
        "the whole tip must scan in one chunk"
    );
    assert!(
        elapsed < SYNC_WALL_CLOCK_CEILING,
        "wallet.sync must not wait on the transparent-UTXO walk's latency, took {elapsed:?}"
    );
    assert_eq!(
        transparent_utxos_calls.load(Ordering::SeqCst),
        0,
        "wallet.sync must never call transparent_utxos"
    );

    let refresh_started = Instant::now();
    let refresh_outcome = wallet.refresh_transparent_utxos(&chain).await?;
    let refresh_elapsed = refresh_started.elapsed();
    let receivers_visited = transparent_utxos_calls.load(Ordering::SeqCst);

    assert!(
        receivers_visited > 1,
        "the fixture wallet must carry more than one transparent receiver for this proof to \
         mean anything, visited {receivers_visited}"
    );
    assert_eq!(
        u64::try_from(receivers_visited).unwrap_or(u64::MAX),
        refresh_outcome.receivers_visited
    );
    assert!(
        refresh_elapsed >= PER_CALL_LATENCY * u32::try_from(receivers_visited).unwrap_or(u32::MAX),
        "the walk itself must still pay every receiver's latency when run directly, visited \
         {receivers_visited} receivers in {refresh_elapsed:?}"
    );
    assert!(
        refresh_elapsed > SYNC_WALL_CLOCK_CEILING,
        "the walk's real cost must exceed what wallet.sync was proven to stay under, took \
         {refresh_elapsed:?}"
    );

    Ok(())
}

/// `ChainSource` that delegates everything to an inner [`MockChainSource`] except
/// `transparent_utxos`, which sleeps `latency` before delegating.
///
/// Stands in for a receiver set whose combined read cost (many receivers, a slow indexer, or
/// both) is `latency`.
struct LatentTransparentUtxoChainSource {
    inner: MockChainSource,
    latency: Duration,
    transparent_utxos_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ChainSource for LatentTransparentUtxoChainSource {
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

    async fn transaction_status(&self, tx_id: TxId) -> Result<TransactionStatus, ChainSourceError> {
        self.inner.transaction_status(tx_id).await
    }

    async fn transparent_utxos(
        &self,
        chain_epoch: ChainEpoch,
        script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        self.transparent_utxos_calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.latency).await;
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

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
