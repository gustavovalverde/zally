//! `WalletStorage::record_idempotent_submission` + `lookup_idempotent_submission` integration.

use tempfile::TempDir;
use zally_core::{IdempotencyKey, TxId};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions, StorageError, WalletStorage};

#[tokio::test]
async fn idempotency_ledger_round_trip() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    let key_a = IdempotencyKey::try_from("invoice-aaaaaaaa")?;
    let key_b = IdempotencyKey::try_from("invoice-bbbbbbbb")?;
    let tx_a = TxId::from_bytes([0x11_u8; 32]);
    let tx_b = TxId::from_bytes([0x22_u8; 32]);

    assert_eq!(storage.lookup_idempotent_submission(&key_a).await?, None);
    storage
        .record_idempotent_submission(key_a.clone(), tx_a)
        .await?;
    assert_eq!(
        storage.lookup_idempotent_submission(&key_a).await?,
        Some(tx_a),
        "record then lookup must return the recorded tx_id",
    );
    assert_eq!(
        storage.lookup_idempotent_submission(&key_b).await?,
        None,
        "an unrelated key must miss",
    );

    storage
        .record_idempotent_submission(key_a.clone(), tx_a)
        .await?;
    let conflict = storage.record_idempotent_submission(key_a, tx_b).await;
    assert!(
        matches!(conflict, Err(StorageError::IdempotencyKeyConflict)),
        "recording the same key with a different tx_id must error: {conflict:?}"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("idempotency key error: {0}")]
    Key(#[from] zally_core::IdempotencyKeyError),
}
