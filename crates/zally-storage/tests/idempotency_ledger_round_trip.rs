//! `WalletStorage::record_idempotent_submission` + `find_idempotent_submission` integration,
//! the terminal state `record_unmineable_transaction` gives a key, and the send record
//! `record_pending_submission` writes before submitting.

use tempfile::TempDir;
use zally_core::{AccountId, BlockHeight, IdempotencyKey, OutPoint, TxId, Zatoshis};
use zally_storage::{PendingBroadcastRecord, Sqlite, SqliteOptions, StorageError, WalletStorage};

#[tokio::test]
async fn idempotency_ledger_round_trip() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    let key_a = IdempotencyKey::try_from("invoice-aaaaaaaa")?;
    let key_b = IdempotencyKey::try_from("invoice-bbbbbbbb")?;
    let tx_a = TxId::from_bytes([0x11_u8; 32]);
    let tx_b = TxId::from_bytes([0x22_u8; 32]);

    assert_eq!(storage.find_idempotent_submission(&key_a).await?, None);
    storage
        .record_idempotent_submission(key_a.clone(), tx_a)
        .await?;
    assert_eq!(
        storage.find_idempotent_submission(&key_a).await?,
        Some(tx_a),
        "record then lookup must return the recorded tx_id",
    );
    assert_eq!(
        storage.find_idempotent_submission(&key_b).await?,
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

#[tokio::test]
async fn a_key_whose_transaction_is_unmineable_binds_its_replacement() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;
    let key = IdempotencyKey::try_from("payout-0001")?;
    let unmineable = TxId::from_bytes([0x11_u8; 32]);
    let replacement = TxId::from_bytes([0x22_u8; 32]);
    let account_id = AccountId::from_uuid(uuid::Uuid::from_bytes([0x33_u8; 16]));
    storage
        .record_idempotent_submission(key.clone(), unmineable)
        .await?;
    storage
        .record_pending_broadcast_inputs(PendingBroadcastRecord {
            broadcast_tx_id: unmineable,
            account_id,
            broadcast_at_ms: 1,
            broadcast_at_height: Some(BlockHeight::from(10)),
            inputs: vec![(
                OutPoint::new(TxId::from_bytes([0x44_u8; 32]), 0),
                Zatoshis::try_from(1_000_u64).unwrap_or(Zatoshis::zero()),
            )],
        })
        .await?;

    storage
        .record_unmineable_transaction(unmineable, BlockHeight::from(20))
        .await?;
    assert_eq!(
        storage
            .record_unmineable_transaction(unmineable, BlockHeight::from(21))
            .await?,
        None,
        "a repeated verdict changes nothing"
    );
    assert_eq!(
        storage.find_idempotent_submission(&key).await?,
        None,
        "an unmineable transaction no longer answers for its key"
    );
    assert!(
        storage
            .list_pending_broadcast_inputs(account_id, 0)
            .await?
            .is_empty(),
        "its pending inputs are dropped with the verdict"
    );

    storage
        .record_idempotent_submission(key.clone(), replacement)
        .await?;
    assert_eq!(
        storage.find_idempotent_submission(&key).await?,
        Some(replacement)
    );
    let conflict = storage.record_idempotent_submission(key, unmineable).await;
    assert!(
        matches!(conflict, Err(StorageError::IdempotencyKeyConflict)),
        "the replacement binds the key until it too is judged unmineable: {conflict:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_pending_submission_binds_the_paying_transaction_until_released() -> Result<(), TestError>
{
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;
    let key = IdempotencyKey::try_from("payout-0002")?;
    let funding = TxId::from_bytes([0x11_u8; 32]);
    let paying = TxId::from_bytes([0x22_u8; 32]);
    let account_id = AccountId::from_uuid(uuid::Uuid::from_bytes([0x33_u8; 16]));
    let record = |broadcast_tx_id: TxId, outpoint_tx_byte: u8| PendingBroadcastRecord {
        broadcast_tx_id,
        account_id,
        broadcast_at_ms: 1,
        broadcast_at_height: Some(BlockHeight::from(10)),
        inputs: vec![(
            OutPoint::new(TxId::from_bytes([outpoint_tx_byte; 32]), 0),
            Zatoshis::try_from(1_000_u64).unwrap_or(Zatoshis::zero()),
        )],
    };
    let pending_broadcasts = || async {
        let mut broadcasts: Vec<TxId> = storage
            .list_pending_broadcast_inputs(account_id, 0)
            .await?
            .into_iter()
            .map(|row| row.broadcast_tx_id)
            .collect();
        broadcasts.sort_by_key(|tx_id| *tx_id.as_bytes());
        Ok::<_, StorageError>(broadcasts)
    };

    storage
        .record_pending_submission(
            key.clone(),
            paying,
            vec![record(funding, 0x44), record(paying, 0x55)],
        )
        .await?;
    assert_eq!(
        storage.find_idempotent_submission(&key).await?,
        Some(paying)
    );
    assert_eq!(pending_broadcasts().await?, vec![funding, paying]);

    let conflict = storage
        .record_pending_submission(key.clone(), funding, vec![record(funding, 0x44)])
        .await;
    assert!(
        matches!(conflict, Err(StorageError::IdempotencyKeyConflict)),
        "a bound key refuses another transaction: {conflict:?}"
    );
    assert_eq!(
        pending_broadcasts().await?,
        vec![funding, paying],
        "the refused record writes nothing"
    );

    let carried_by_a_rebuild = rusqlite::Connection::open(temp.path().join("wallet.db"))?;
    carried_by_a_rebuild.execute(
        "INSERT INTO ext_zally_reverted_broadcasts \
             (tx_id_bytes, raw_tx_bytes, expiry_height, recorded_at_ms) \
         VALUES (?1, x'00', 100, 1)",
        [paying.as_bytes().as_slice()],
    )?;
    drop(carried_by_a_rebuild);

    storage.release_pending_submission(vec![paying]).await?;
    assert_eq!(storage.find_idempotent_submission(&key).await?, None);
    assert!(
        storage.list_reverted_broadcasts().await?.is_empty(),
        "a released transaction is never resubmitted from the reverted-broadcast ledger"
    );
    assert_eq!(
        pending_broadcasts().await?,
        vec![funding],
        "a step that may be in flight keeps its pending inputs"
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
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}
