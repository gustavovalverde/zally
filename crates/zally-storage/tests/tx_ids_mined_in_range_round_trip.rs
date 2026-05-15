//! Round trip for [`WalletStorage::wallet_tx_ids_mined_in_range`].
//!
//! Opens a fresh wallet DB (lets the upstream migrations run), then writes synthetic rows
//! into the upstream `transactions` table via a side rusqlite connection and asserts the
//! Zally query surface returns exactly the entries that fall in the requested range.

use rusqlite::params;
use tempfile::TempDir;
use zally_core::{BlockHeight, TxId};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions, StorageError, WalletStorage};

#[tokio::test]
async fn wallet_tx_ids_mined_in_range_round_trip() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let db_path = temp.path().join("wallet.db");
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        zally_core::Network::regtest(),
        db_path.clone(),
    ));
    storage.open_or_create().await?;

    let mined_at = [
        (TxId::from_bytes([0xAA_u8; 32]), 5_i64),
        (TxId::from_bytes([0xBB_u8; 32]), 10_i64),
        (TxId::from_bytes([0xCC_u8; 32]), 15_i64),
        (TxId::from_bytes([0xDD_u8; 32]), 25_i64),
    ];
    write_synthetic_transactions(&db_path, &mined_at)?;

    let in_window = storage
        .wallet_tx_ids_mined_in_range(BlockHeight::from(7), BlockHeight::from(20))
        .await?;
    let expected: Vec<(TxId, BlockHeight)> = mined_at[1..=2]
        .iter()
        .map(|(tx, h)| (*tx, BlockHeight::from(u32::try_from(*h).unwrap_or(0))))
        .collect();
    assert_eq!(
        in_window, expected,
        "range [7, 20] must return only the txs mined at 10 and 15"
    );

    let nothing = storage
        .wallet_tx_ids_mined_in_range(BlockHeight::from(30), BlockHeight::from(40))
        .await?;
    assert!(
        nothing.is_empty(),
        "range with no rows must return an empty vec, got {nothing:?}"
    );

    let single = storage
        .wallet_tx_ids_mined_in_range(BlockHeight::from(25), BlockHeight::from(25))
        .await?;
    assert_eq!(
        single,
        vec![(mined_at[3].0, BlockHeight::from(25))],
        "inclusive single-height range must hit"
    );
    Ok(())
}

fn write_synthetic_transactions(
    db_path: &std::path::Path,
    rows: &[(TxId, i64)],
) -> Result<(), rusqlite::Error> {
    let conn = rusqlite::Connection::open(db_path)?;
    let block_min: i64 = rows.iter().map(|(_, h)| *h).min().unwrap_or(1);
    let block_max: i64 = rows.iter().map(|(_, h)| *h).max().unwrap_or(1);
    for height in block_min..=block_max {
        conn.execute(
            "INSERT OR IGNORE INTO blocks \
                 (height, hash, time, sapling_tree, sapling_commitment_tree_size, \
                  sapling_output_count, orchard_commitment_tree_size, orchard_action_count) \
             VALUES (?1, ?2, ?3, ?4, 0, 0, 0, 0)",
            params![height, vec![0_u8; 32], 0_i64, Vec::<u8>::new()],
        )?;
    }
    for (tx_id, height) in rows {
        conn.execute(
            "INSERT INTO transactions \
                 (txid, block, mined_height, min_observed_height) \
             VALUES (?1, ?2, ?3, ?4)",
            params![tx_id.as_bytes().to_vec(), *height, *height, *height],
        )?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("rusqlite error: {0}")]
    Rusqlite(#[from] rusqlite::Error),
}
