//! `Wallet::metrics_snapshot` returns a typed snapshot of wallet state (OBS-2).

use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zally_wallet::SyncStatus;

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn metrics_snapshot_reports_network_and_account_count() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let snapshot = wallet.metrics_snapshot().await?;
    assert_eq!(snapshot.network, network);
    assert_eq!(snapshot.account_count, 1);
    assert_eq!(snapshot.scanned_height, None);
    assert_eq!(snapshot.visible_tip_height, None);
    assert_eq!(snapshot.settled_tip_height, None);
    assert_eq!(snapshot.lag_blocks, None);

    // Attaching an observer reflects in the snapshot.
    let _events = wallet.observe();
    let snapshot_after = wallet.metrics_snapshot().await?;
    assert!(snapshot_after.event_subscriber_count >= 1);
    Ok(())
}

#[tokio::test]
async fn sync_scans_to_visible_tip_and_records_the_lower_settled_tip() -> Result<(), TestWalletError>
{
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let chain = MockChainSource::new(network);
    assert!(
        chain
            .handle()
            .set_chain_tips(BlockHeight::from(50), BlockHeight::from(42),)
    );
    wallet.sync(&chain).await?;

    let status = wallet.status_snapshot().await?;
    assert_eq!(status.network, network);
    assert_eq!(status.scanned_height, Some(BlockHeight::from(50)));
    assert_eq!(status.visible_tip_height, Some(BlockHeight::from(50)));
    assert_eq!(status.settled_tip_height, Some(BlockHeight::from(42)));
    assert_eq!(
        status.sync_status,
        SyncStatus::AtTip {
            visible_tip_height: BlockHeight::from(50)
        }
    );
    assert_eq!(status.lag_blocks, Some(0));

    let metrics = wallet.metrics_snapshot().await?;
    assert_eq!(metrics.visible_tip_height, Some(BlockHeight::from(50)));
    assert_eq!(metrics.settled_tip_height, Some(BlockHeight::from(42)));
    assert_eq!(metrics.lag_blocks, Some(0));
    Ok(())
}
