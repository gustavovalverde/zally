//! A caught-up wallet's balance holds while a sync scans the next block.
//!
//! `zcash_client_sqlite` gives no note a witness while blocks of its commitment-tree shard are
//! unscanned, so a chain tip recorded before its blocks are scanned would take every note of
//! the tip shard out of the balance and out of proposals until the scan caught up. The wallet
//! records the new tip in the same storage step as the scan of the blocks that reach it.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zally_chain::{
    BlockHeightRange, ChainEpoch, ChainEventEnvelopeStream, ChainEventStreamStart, ChainSource,
    ChainSourceError, CompactBlockStream, ShieldedPool, SubtreeIndex, SubtreeRoot,
    TransactionStatus, TransparentUtxo,
};
use zally_core::{AccountId, BlockHeight, Network, TreeStateArtifact, TxId};
use zally_testkit::MockChainSource;
use zally_wallet::Wallet;

use super::reorg_fixtures::{FixtureError, funded_chain, sync_to_tip};

/// Blocks above the funded tip that give the shielded note its confirmations.
const CONFIRMATION_BLOCKS: u32 = 20;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spendable_balance_holds_while_the_next_block_is_scanned() -> Result<(), FixtureError> {
    let funded = funded_chain(200).await?;
    let base = &funded.base;
    let mut tip_height = funded.tip_height + CONFIRMATION_BLOCKS;
    base.chain_handle.advance_tip(BlockHeight::from(tip_height));
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    let spendable = base
        .wallet
        .spendable_for_next_dispense(base.account_id)
        .await?;
    assert!(spendable.as_u64() > 0, "the funded note is spendable");

    let chain = BalanceReadingChain {
        inner: Arc::clone(&base.chain),
        wallet: base.wallet.clone(),
        account_id: base.account_id,
        balances_while_fetching: Mutex::new(Vec::new()),
    };
    for _ in 0..3 {
        tip_height += 1;
        base.chain_handle.advance_tip(BlockHeight::from(tip_height));
        let outcome = base.wallet.sync(&chain).await?;
        assert_eq!(outcome.scanned_to_height, BlockHeight::from(tip_height));
    }

    let balances = chain.balances_while_fetching.lock().clone();
    assert_eq!(balances.len(), 3, "each sync fetched its new block");
    assert!(
        balances
            .iter()
            .all(|balance| *balance == spendable.as_u64()),
        "a read during the scan sees the balance before it, got {balances:?} against {}",
        spendable.as_u64()
    );
    assert_eq!(
        base.wallet
            .spendable_for_next_dispense(base.account_id)
            .await?,
        spendable
    );
    Ok(())
}

/// Delegates to a [`MockChainSource`] and reads the wallet's spendable balance each time a
/// sync fetches compact blocks, between its plan and its scan.
struct BalanceReadingChain {
    inner: Arc<MockChainSource>,
    wallet: Wallet,
    account_id: AccountId,
    balances_while_fetching: Mutex<Vec<u64>>,
}

#[async_trait]
impl ChainSource for BalanceReadingChain {
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
        let balance = self
            .wallet
            .spendable_for_next_dispense(self.account_id)
            .await
            .map_err(|_| ChainSourceError::UnsupportedResponse {
                response: "spendable balance read failed",
            })?;
        self.balances_while_fetching.lock().push(balance.as_u64());
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
