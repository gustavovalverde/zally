//! Storage-level integration: `WalletStorage::list_shielded_receives_for_account` against
//! an empty wallet returns `Ok(vec![])` and accepts both a freshly-created account and an
//! unknown account UUID without panicking.
//!
//! Funded coverage (real Sapling/Orchard receives round-tripping through the new query
//! with `is_change` and `spent_our_inputs` populated) lands once fauzec's staging soak
//! provides a deterministic on-chain fixture, for the same reason
//! `list_unspent_shielded_notes_round_trip.rs` defers funded coverage: inserting synthetic
//! rows directly into the per-pool received-notes tables would bypass the upstream's
//! account-membership and spend-tracking invariants and produce false positives.

use tempfile::TempDir;
use zally_core::AccountId;
use zally_keys::{Mnemonic, SeedMaterial};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions, StorageError, WalletStorage};
use zcash_client_backend::data_api::chain::ChainState;
use zcash_primitives::block::BlockHash;

#[tokio::test]
async fn list_shielded_receives_round_trip_empty_account() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    let mnemonic = Mnemonic::generate();
    let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
    let account_id = storage
        .create_account_for_seed(&seed, ChainState::empty(0.into(), BlockHash([0u8; 32])))
        .await?;

    let rows = storage
        .list_shielded_receives_for_account(account_id)
        .await?;
    assert!(
        rows.is_empty(),
        "fresh account must have no historical receives, got {rows:?}"
    );
    Ok(())
}

#[tokio::test]
async fn list_shielded_receives_returns_empty_for_unknown_account() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    let unknown_account = AccountId::from_uuid(uuid::Uuid::new_v4());
    let rows = storage
        .list_shielded_receives_for_account(unknown_account)
        .await?;
    assert!(
        rows.is_empty(),
        "unknown account must yield no rows, got {rows:?}"
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
