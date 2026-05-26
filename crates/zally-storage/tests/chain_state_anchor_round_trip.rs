//! `WalletStorage::record_chain_state_anchor` and its companion lookup and prune methods
//! form the cache the wallet consults on resume before falling back to a chain-source
//! `tree_state_at` RPC. The contract: stores opaque bytes, returns the highest cached
//! row at-or-below a target height, and prunes rows strictly below a floor.

#![allow(
    clippy::expect_used,
    reason = "test fixtures assert on exact shapes; expect() with descriptive messages keeps failure mode readable"
)]

use tempfile::TempDir;
use zally_core::BlockHeight;
use zally_storage::{Sqlite, SqliteOptions, StorageError, WalletStorage};

#[tokio::test]
async fn chain_state_anchor_round_trip_persists_opaque_bytes() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    let payload = b"opaque tree-state bytes".to_vec();
    storage
        .record_chain_state_anchor(BlockHeight::from(100), payload.clone(), 1_700_000_000_000)
        .await?;

    let (height, fetched) = storage
        .find_chain_state_anchor_at_or_below(BlockHeight::from(100))
        .await?
        .expect("anchor at the requested height must be returned");
    assert_eq!(height, BlockHeight::from(100));
    assert_eq!(fetched, payload);

    Ok(())
}

#[tokio::test]
async fn chain_state_anchor_returns_highest_at_or_below() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    for height in [50_u32, 100, 150, 200] {
        storage
            .record_chain_state_anchor(
                BlockHeight::from(height),
                format!("payload@{height}").into_bytes(),
                1_700_000_000_000,
            )
            .await?;
    }

    let (height, payload) = storage
        .find_chain_state_anchor_at_or_below(BlockHeight::from(175))
        .await?
        .expect("anchor at-or-below 175 must exist");
    assert_eq!(
        height,
        BlockHeight::from(150),
        "the highest anchor not above the target is 150",
    );
    assert_eq!(payload, b"payload@150");

    let exact = storage
        .find_chain_state_anchor_at_or_below(BlockHeight::from(50))
        .await?
        .expect("exact match at the lowest anchor must be returned");
    assert_eq!(exact.0, BlockHeight::from(50));

    assert!(
        storage
            .find_chain_state_anchor_at_or_below(BlockHeight::from(49))
            .await?
            .is_none(),
        "querying below every stored anchor returns None"
    );

    Ok(())
}

#[tokio::test]
async fn chain_state_anchor_record_overwrites_existing_row() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    storage
        .record_chain_state_anchor(BlockHeight::from(100), b"first".to_vec(), 1_700_000_000_000)
        .await?;
    storage
        .record_chain_state_anchor(BlockHeight::from(100), b"second".to_vec(), 1_700_000_001_000)
        .await?;

    let (_height, payload) = storage
        .find_chain_state_anchor_at_or_below(BlockHeight::from(100))
        .await?
        .expect("row must still exist");
    assert_eq!(
        payload, b"second",
        "a second insert at the same height must overwrite the first row"
    );

    Ok(())
}

#[tokio::test]
async fn chain_state_anchor_prune_drops_rows_strictly_below_floor() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    for height in [50_u32, 100, 150, 200] {
        storage
            .record_chain_state_anchor(
                BlockHeight::from(height),
                format!("payload@{height}").into_bytes(),
                1_700_000_000_000,
            )
            .await?;
    }

    storage
        .prune_chain_state_anchors_below(BlockHeight::from(100))
        .await?;

    assert!(
        storage
            .find_chain_state_anchor_at_or_below(BlockHeight::from(99))
            .await?
            .is_none(),
        "anchors below the floor are gone"
    );
    let (kept_height, _) = storage
        .find_chain_state_anchor_at_or_below(BlockHeight::from(100))
        .await?
        .expect("floor-height anchor must survive (DELETE uses strict less-than)");
    assert_eq!(kept_height, BlockHeight::from(100));
    let (top, _) = storage
        .find_chain_state_anchor_at_or_below(BlockHeight::from(200))
        .await?
        .expect("anchors above the floor are untouched");
    assert_eq!(top, BlockHeight::from(200));

    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}
