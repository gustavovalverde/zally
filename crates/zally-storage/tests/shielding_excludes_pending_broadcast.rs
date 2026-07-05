//! Regression test for the pending-broadcast exclusion filter on the shielding path.
//!
//! `propose_shielding` gathers transparent inputs through
//! `InputSource::get_spendable_transparent_outputs_for_addresses`, not the
//! single-address `get_spendable_transparent_outputs` method. `FilteredWalletDb`
//! must apply its exclusion filter along that batched path too, by inheriting the
//! trait default (which fans out to the per-address override) rather than
//! delegating the batched method straight to the inner `WalletDb`.

use std::collections::HashSet;

use tempfile::TempDir;
use zally_core::{BlockHeight, OutPoint, TxId, Zatoshis};
use zally_keys::{Mnemonic, SeedMaterial};
use zally_storage::{
    ShieldTransparentRequest, Sqlite, SqliteOptions, StorageError, TransparentUtxoRow,
    WalletStorage,
};
use zcash_client_backend::data_api::chain::ChainState;
use zcash_primitives::block::BlockHash;
use zcash_transparent::address::Script;

#[tokio::test]
async fn shielding_skips_excluded_outpoint_via_batched_gather() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
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
    let script_pub_key_bytes = Script::from(transparent.script()).0.0;

    let mined_height = BlockHeight::from(2);
    storage.update_chain_tip(BlockHeight::from(200)).await?;

    let excluded_tx_id = TxId::from_bytes([0x11_u8; 32]);
    let kept_tx_id = TxId::from_bytes([0x22_u8; 32]);
    let utxo_amount = Zatoshis::try_from(1_000_000_u64).unwrap_or(Zatoshis::zero());
    storage
        .record_transparent_utxos(vec![
            TransparentUtxoRow::new(
                excluded_tx_id,
                0,
                utxo_amount,
                mined_height,
                script_pub_key_bytes.clone(),
            ),
            TransparentUtxoRow::new(
                kept_tx_id,
                0,
                utxo_amount,
                mined_height,
                script_pub_key_bytes,
            ),
        ])
        .await?;

    let excluded_outpoint = OutPoint::new(excluded_tx_id, 0);
    let mut excluded_outpoints = HashSet::new();
    excluded_outpoints.insert(excluded_outpoint);

    let threshold = Zatoshis::try_from(1_000_u64).unwrap_or(Zatoshis::zero());
    let prepared = storage
        .shield_transparent_funds(
            ShieldTransparentRequest::new(account_id, threshold),
            excluded_outpoints,
            &seed,
        )
        .await?;

    let transparent_inputs: Vec<OutPoint> = prepared
        .iter()
        .flat_map(|tx| tx.transparent_inputs.iter().map(|(outpoint, _)| *outpoint))
        .collect();
    assert!(
        !transparent_inputs.contains(&excluded_outpoint),
        "shielding must not spend an outpoint locked by a pending broadcast: {transparent_inputs:?}"
    );
    assert!(
        transparent_inputs.contains(&OutPoint::new(kept_tx_id, 0)),
        "shielding must still spend the non-excluded outpoint: {transparent_inputs:?}"
    );
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
