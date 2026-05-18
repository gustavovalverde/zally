//! `Wallet::observe()` receives `ScanProgress` events emitted during sync.

use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zally_wallet::WalletEvent;

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn observe_emits_scan_progress() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let mut events = wallet.observe();
    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(7));
    let _ = wallet.sync(&chain).await?;

    let first = events.next().await;
    assert!(matches!(first, Some(WalletEvent::ScanProgress { .. })));
    let second = events.next().await;
    assert!(matches!(
        second,
        Some(WalletEvent::ScanProgress {
            scanned_height,
            target_height,
        }) if scanned_height == BlockHeight::from(7) && target_height == BlockHeight::from(7)
    ));
    Ok(())
}
