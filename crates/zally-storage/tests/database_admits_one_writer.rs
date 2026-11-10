//! A wallet database admits one writing storage handle at a time.
//!
//! A second handle on the same file fails to open with `StorageError::DatabaseLocked`, while
//! clones of the open handle share it. Once every clone of the writer is dropped, the next
//! handle opens the database.

use tempfile::TempDir;
use zally_core::{FailurePosture, Network};
use zally_storage::{Sqlite, SqliteOptions, StorageError, WalletStorage};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_storage_handle_cannot_open_a_database_another_holds() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let db_path = temp.path().join("wallet.db");
    let writer = Sqlite::new(SqliteOptions::for_network(
        Network::regtest(),
        db_path.clone(),
    ));
    writer.open_or_create().await?;
    let writer_clone = writer.clone();
    writer_clone.open_or_create().await?;

    let second = Sqlite::new(SqliteOptions::for_network(
        Network::regtest(),
        db_path.clone(),
    ));
    let refused = second.open_or_create().await;
    assert!(
        matches!(
            &refused,
            Err(StorageError::DatabaseLocked { db_path: locked }) if *locked == db_path
        ),
        "a second writer is refused: {refused:?}"
    );
    assert!(
        matches!(
            refused.as_ref().map_err(StorageError::posture),
            Err(FailurePosture::RequiresOperator)
        ),
        "the operator must stop the other writer"
    );

    drop(writer);
    drop(writer_clone);
    second.open_or_create().await?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}
