//! Regression: `Wallet::sync` emits `WalletEvent::ReorgDetected` when the chain tip is
//! observed below the wallet's prior fully-scanned height across syncs.

use zally_chain::ChainSource as _;
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{MockChainSource, TempWalletPath};
use zally_wallet::{Wallet, WalletError, WalletEvent};

#[tokio::test]
async fn sync_emits_reorg_when_tip_regresses() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, _account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;
    let mut events = wallet.observe();

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    wallet.sync(&chain).await?;

    chain.handle().advance_tip(BlockHeight::from(20));
    let final_tip = chain.chain_tip().await.map_err(|err| TestError::Chain {
        reason: err.to_string(),
    })?;
    assert_eq!(final_tip.as_u32(), 20, "mock must report the regressed tip");
    let outcome = wallet.sync(&chain).await?;
    assert_eq!(
        outcome.reorgs_observed, 1,
        "the second sync must observe one reorg via tip regression"
    );

    let mut reorg_event = None;
    while let Some(event) = events.next().await {
        if let WalletEvent::ReorgDetected {
            rolled_back_to_height,
            new_tip_height,
        } = event
        {
            reorg_event = Some((rolled_back_to_height, new_tip_height));
            break;
        }
    }
    let (rolled_back_to_height, new_tip_height) = reorg_event.ok_or(TestError::NoReorgEvent)?;
    assert_eq!(rolled_back_to_height.as_u32(), 20);
    assert_eq!(new_tip_height.as_u32(), 20);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("chain source error: {reason}")]
    Chain { reason: String },
    #[error("no ReorgDetected event observed")]
    NoReorgEvent,
}
