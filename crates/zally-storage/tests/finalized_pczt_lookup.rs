//! Finalized PCZT lookup through the public storage boundary.

use tempfile::TempDir;
use zally_core::{Network, TxId};
use zally_storage::{Sqlite, SqliteOptions, StorageError, WalletStorage};

#[tokio::test]
async fn finalized_pczt_lookup_misses_for_unknown_transaction() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    let finalized_pczt_bytes = storage
        .find_finalized_pczt_bytes(TxId::from_bytes([0x35; 32]))
        .await?;
    assert_eq!(finalized_pczt_bytes, None);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}
