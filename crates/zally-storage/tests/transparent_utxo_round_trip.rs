//! Storage-level transparent receiver and UTXO round trip.

use tempfile::TempDir;
use zally_core::{BlockHeight, TxId};
use zally_keys::{Mnemonic, SeedMaterial};
use zally_storage::{
    SqliteWalletStorage, SqliteWalletStorageOptions, StorageError, TransparentUtxoRow,
    WalletStorage,
};
use zcash_client_backend::data_api::chain::ChainState;
use zcash_primitives::block::BlockHash;
use zcash_transparent::address::Script;

#[tokio::test]
async fn transparent_utxo_round_trip_records_exposed_receiver() -> Result<(), TestError> {
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
    let ua = storage
        .derive_next_address_with_transparent(account_id)
        .await?;
    let transparent = ua
        .transparent()
        .copied()
        .ok_or(TestError::TransparentReceiverMissing)?;
    let expected_script = Script::from(transparent.script()).0.0;

    let receivers = storage.list_transparent_receivers().await?;
    assert!(
        receivers
            .iter()
            .any(|receiver| receiver.account_id == account_id
                && receiver.script_pub_key_bytes == expected_script),
        "derived transparent receiver must be returned for sync refresh"
    );

    storage.update_chain_tip(BlockHeight::from(10)).await?;
    let recorded_count = storage
        .record_transparent_utxos(vec![TransparentUtxoRow::new(
            TxId::from_bytes([0x11_u8; 32]),
            0,
            50_000,
            BlockHeight::from(2),
            expected_script,
        )])
        .await?;
    assert_eq!(recorded_count, 1);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("derived UA did not contain a transparent receiver")]
    TransparentReceiverMissing,
}
