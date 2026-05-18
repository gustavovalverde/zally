//! `Wallet::sync` advances scan progress to the chain tip (SYNC-1).

use zally_core::BlockHeight;
use zally_testkit::MockChainSource;

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn sync_catches_up_to_tip() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(42));

    let outcome = wallet.sync(&chain).await?;
    assert_eq!(outcome.scanned_to_height, BlockHeight::from(42));
    assert_eq!(outcome.reorgs_observed, 0);
    Ok(())
}
