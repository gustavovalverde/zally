//! REQ-SYNC — `Wallet::sync` scans only to the chain source's finalized height, never to
//! the raw tip. Stopping below the reorg window is what makes orphan-caching impossible.

use zally_core::{BlockHeight, Network};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, MockChainSource, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn sync_stops_at_finalized_height() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = InMemorySealing::new();
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let chain = MockChainSource::new(network);
    let (wallet, _, _) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    // Raw tip is 50, but only 40 is finalized: the wallet must stop at 40.
    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain.handle().set_finalized_height(BlockHeight::from(40));

    let outcome = wallet.sync(&chain).await?;
    assert_eq!(
        outcome.scanned_to_height,
        BlockHeight::from(40),
        "sync must stop at the finalized height, not the raw tip",
    );

    // Once finality advances to the tip, the wallet catches up the rest of the way.
    chain.handle().set_finalized_height(BlockHeight::from(50));
    let outcome = wallet.sync(&chain).await?;
    assert_eq!(outcome.scanned_to_height, BlockHeight::from(50));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
