//! `WalletStorage::record_chain_tips` records the most recent canonical tips
//! unconditionally. A regress must overwrite a higher prior value: a monotonic high-water
//! mark would hide a tip regress and break reorg detection.

use tempfile::TempDir;
use zally_core::BlockHeight;
use zally_storage::{Sqlite, SqliteOptions, StorageError, WalletStorage};

#[tokio::test]
async fn chain_tips_record_regress() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    assert_eq!(
        storage.find_visible_tip().await?,
        None,
        "a fresh wallet has no observed tip",
    );

    storage
        .record_chain_tips(BlockHeight::from(50), BlockHeight::from(40))
        .await?;
    assert_eq!(
        storage.find_visible_tip().await?,
        Some(BlockHeight::from(50)),
    );
    assert_eq!(
        storage.find_settled_tip().await?,
        Some(BlockHeight::from(40)),
    );

    let invalid = storage
        .record_chain_tips(BlockHeight::from(40), BlockHeight::from(41))
        .await;
    assert!(matches!(
        invalid,
        Err(StorageError::InvalidChainTips { .. })
    ));
    assert_eq!(
        storage.find_visible_tip().await?,
        Some(BlockHeight::from(50)),
        "an invalid pair must not partially mutate the visible tip",
    );
    assert_eq!(
        storage.find_settled_tip().await?,
        Some(BlockHeight::from(40)),
        "an invalid pair must not partially mutate the settled tip",
    );

    // A regress (the chain source reporting a lower tip after a reorg) must overwrite the
    // higher prior value, not be swallowed.
    storage
        .record_chain_tips(BlockHeight::from(20), BlockHeight::from(10))
        .await?;
    assert_eq!(
        storage.find_visible_tip().await?,
        Some(BlockHeight::from(20)),
        "record_chain_tips must store the regressed visible tip, not keep the high-water mark",
    );
    assert_eq!(
        storage.find_settled_tip().await?,
        Some(BlockHeight::from(10)),
    );

    storage
        .record_chain_tips(BlockHeight::from(30), BlockHeight::from(20))
        .await?;
    assert_eq!(
        storage.find_visible_tip().await?,
        Some(BlockHeight::from(30)),
    );
    Ok(())
}

#[tokio::test]
async fn opening_retires_ambiguous_legacy_observed_tip() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let db_path = temp.path().join("wallet.db");
    let legacy = rusqlite::Connection::open(&db_path)?;
    legacy.execute_batch(
        "CREATE TABLE ext_zally_observed_tip (\
             id INTEGER PRIMARY KEY CHECK (id = 0),\
             tip_height INTEGER NOT NULL\
         );\
         INSERT INTO ext_zally_observed_tip (id, tip_height) VALUES (0, 50);",
    )?;
    drop(legacy);

    let storage = Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        db_path.clone(),
    ));
    storage.open_or_create().await?;
    assert_eq!(storage.find_chain_tips().await?, None);
    drop(storage);

    let reopened = rusqlite::Connection::open(db_path)?;
    let legacy_table_count: i64 = reopened.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'ext_zally_observed_tip'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(legacy_table_count, 0);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}
