//! Storage-level integration: `WalletStorage::list_unspent_shielded_notes` against an empty
//! wallet returns `Ok(vec![])` and propagates the configured target height into the
//! upstream query without panicking.
//!
//! Funded-wallet coverage (a real Sapling/Orchard unspent note round-tripping through the
//! upstream `InputSource::select_unspent_notes` query) lands once fauzec's phase 2 staging
//! soak provides a deterministic on-chain fixture. Inserting synthetic rows directly into
//! `sapling_received_notes` / `orchard_received_notes` would bypass the upstream's
//! account-membership invariants and produce false positives, so this slice tests the
//! empty-account branch only and defers funded coverage to phase 2.

use tempfile::TempDir;
use zally_core::{AccountId, BlockHeight};
use zally_keys::{Mnemonic, SeedMaterial};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions, StorageError, WalletStorage};
use zcash_client_backend::data_api::chain::ChainState;
use zcash_primitives::block::BlockHash;

#[tokio::test]
async fn list_unspent_shielded_notes_round_trip_empty_account() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    let mnemonic = Mnemonic::generate();
    let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
    let account_id = storage
        .create_account_for_seed(&seed, ChainState::empty(0.into(), BlockHash([0u8; 32])))
        .await?;

    let rows = storage
        .list_unspent_shielded_notes(account_id, BlockHeight::from(100))
        .await?;
    assert!(
        rows.is_empty(),
        "fresh account must have no unspent notes, got {rows:?}"
    );
    Ok(())
}

#[tokio::test]
async fn list_unspent_shielded_notes_returns_empty_for_unknown_account() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;

    let unknown = AccountId::from_uuid(uuid::Uuid::nil());
    let rows = storage
        .list_unspent_shielded_notes(unknown, BlockHeight::from(100))
        .await?;
    assert!(rows.is_empty());
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}
