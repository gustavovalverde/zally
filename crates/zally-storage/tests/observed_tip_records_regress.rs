//! `WalletStorage::record_observed_tip` records the most recently observed tip
//! unconditionally. A regress must overwrite a higher prior value: a monotonic high-water
//! mark would hide a tip regress and break reorg detection.

use tempfile::TempDir;
use zally_core::BlockHeight;
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions, StorageError, WalletStorage};

#[tokio::test]
async fn observed_tip_records_regress() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    assert_eq!(
        storage.lookup_observed_tip().await?,
        None,
        "a fresh wallet has no observed tip",
    );

    storage.record_observed_tip(BlockHeight::from(50)).await?;
    assert_eq!(
        storage.lookup_observed_tip().await?,
        Some(BlockHeight::from(50)),
    );

    // A regress (the chain source reporting a lower tip after a reorg) must overwrite the
    // higher prior value, not be swallowed.
    storage.record_observed_tip(BlockHeight::from(20)).await?;
    assert_eq!(
        storage.lookup_observed_tip().await?,
        Some(BlockHeight::from(20)),
        "record_observed_tip must store the regressed tip, not keep the high-water mark",
    );

    storage.record_observed_tip(BlockHeight::from(30)).await?;
    assert_eq!(
        storage.lookup_observed_tip().await?,
        Some(BlockHeight::from(30)),
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}
