//! `SyncDriver` follows chain events without making callers write their own loop.

use std::future;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream;
use zally_chain::{
    BlockHeightRange, ChainEventCursor, ChainEventEnvelope, ChainEventEnvelopeStream, ChainSource,
    ChainSourceError, CompactBlockStream, ShieldedPool, SubtreeIndex, SubtreeRoot,
    TransactionStatus, TransparentUtxo,
};
use zally_core::{BlockHeight, Network, TxId};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, MockChainSource, TempWalletPath};
use zally_wallet::{
    SyncDriver, SyncDriverOptions, SyncDriverStatus, SyncStatus, Wallet, WalletError,
};
use zcash_client_backend::proto::service::TreeState;

#[tokio::test]
async fn sync_driver_wakes_from_chain_event() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();

    let sealing = InMemorySealing::new();
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = Arc::new(MockChainSource::new(network));
    let (wallet, _, _) = Wallet::create(
        chain.as_ref(),
        network,
        sealing,
        storage,
        BlockHeight::from(1),
    )
    .await?;

    let chain_handle = chain.handle();
    let chain_source: Arc<dyn ChainSource> = chain;
    let driver = SyncDriver::new(
        wallet,
        chain_source,
        SyncDriverOptions::default()
            .with_poll_interval_ms(60_000)
            .with_max_sync_iterations_per_wake_count(4),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    wait_for_driver_status(&mut snapshots, SyncDriverStatus::Waiting).await?;
    chain_handle.advance_tip(BlockHeight::from(42));
    let observed = wait_for_chain_tip(&mut snapshots, BlockHeight::from(42)).await?;

    assert_eq!(observed.chain_tip_height, Some(BlockHeight::from(42)));
    assert_eq!(
        observed.sync_status,
        SyncStatus::Starting {
            target_height: BlockHeight::from(42)
        }
    );
    assert_eq!(observed.last_error, None);

    handle.close().await?;
    Ok(())
}

#[tokio::test]
async fn close_returns_while_sync_attempt_is_blocked() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();

    let sealing = InMemorySealing::new();
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = MockChainSource::new(network);
    let (wallet, _, _) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    let chain_source: Arc<dyn ChainSource> = Arc::new(StalledChainSource::new(network));
    let driver = SyncDriver::new(
        wallet,
        chain_source,
        SyncDriverOptions::default()
            .with_poll_interval_ms(60_000)
            .with_sync_timeout_seconds(60),
    )?;
    let handle = driver.sync_continuously();
    let mut snapshots = handle.observe_status();

    wait_for_driver_status(&mut snapshots, SyncDriverStatus::Syncing).await?;
    tokio::time::timeout(Duration::from_millis(250), handle.close())
        .await
        .map_err(|_elapsed| TestError::CloseTimedOut)??;

    Ok(())
}

async fn wait_for_driver_status(
    snapshots: &mut zally_wallet::SyncSnapshotStream,
    target_status: SyncDriverStatus,
) -> Result<(), TestError> {
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(snapshot) = snapshots.next().await {
            if snapshot.driver_status == target_status {
                return Ok(());
            }
        }
        Err(TestError::SnapshotStreamClosed)
    })
    .await
    .map_err(|_| TestError::SnapshotTimeout)?
}

async fn wait_for_chain_tip(
    snapshots: &mut zally_wallet::SyncSnapshotStream,
    target_height: BlockHeight,
) -> Result<zally_wallet::SyncSnapshot, TestError> {
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(snapshot) = snapshots.next().await {
            if snapshot.chain_tip_height == Some(target_height) {
                return Ok(snapshot);
            }
        }
        Err(TestError::SnapshotStreamClosed)
    })
    .await
    .map_err(|_| TestError::SnapshotTimeout)?
}

struct StalledChainSource {
    network: Network,
}

impl StalledChainSource {
    const fn new(network: Network) -> Self {
        Self { network }
    }
}

#[async_trait::async_trait]
impl ChainSource for StalledChainSource {
    fn network(&self) -> Network {
        self.network
    }

    async fn chain_tip(&self) -> Result<BlockHeight, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn compact_blocks(
        &self,
        _block_range: BlockHeightRange,
    ) -> Result<CompactBlockStream, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn tree_state_at(
        &self,
        _block_height: BlockHeight,
    ) -> Result<TreeState, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn subtree_roots(
        &self,
        _pool: ShieldedPool,
        _start_index: SubtreeIndex,
        _max_count: u32,
    ) -> Result<Vec<SubtreeRoot>, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn transaction_status(
        &self,
        _tx_id: TxId,
    ) -> Result<TransactionStatus, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn transparent_utxos(
        &self,
        _script_pub_key_bytes: &[u8],
    ) -> Result<Vec<TransparentUtxo>, ChainSourceError> {
        let _ = self.network;
        future::pending().await
    }

    async fn chain_event_envelopes(
        &self,
        _from_cursor: Option<ChainEventCursor>,
    ) -> Result<ChainEventEnvelopeStream, ChainSourceError> {
        let _ = self.network;
        tokio::task::yield_now().await;
        Ok(Box::pin(stream::pending::<
            Result<ChainEventEnvelope, ChainSourceError>,
        >()))
    }
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("sync snapshot stream closed")]
    SnapshotStreamClosed,
    #[error("timed out waiting for sync snapshot")]
    SnapshotTimeout,
    #[error("timed out waiting for sync driver close")]
    CloseTimedOut,
}
