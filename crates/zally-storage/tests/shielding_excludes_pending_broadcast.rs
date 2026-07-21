//! Regression test for the pending-broadcast exclusion filter on the shielding path.
//!
//! `propose_shielding` gathers transparent inputs through
//! `InputSource::get_spendable_transparent_outputs_for_addresses`, not the
//! single-address `get_spendable_transparent_outputs` method. `FilteredWalletDb`
//! must apply its exclusion filter along that batched path too, by inheriting the
//! trait default (which fans out to the per-address override) rather than
//! delegating the batched method straight to the inner `WalletDb`.
//!
//! The wallet must scan at least one block before any proposal can be built: proposals
//! anchor at a note commitment tree checkpoint, and checkpoints exist only for scanned
//! blocks. The fixture scans a slice of the vendored testnet compact blocks (see
//! `commitment_tree_roots_regress.rs` for the fixture provenance) before shielding.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use prost::Message as _;
use tempfile::TempDir;
#[path = "fixtures/sapling_proving_parameters.rs"]
mod sapling_proving_parameters;
#[path = "fixtures/scan_artifact.rs"]
mod scan_artifact;

use zally_core::{
    BlockHeight, CompactBlockArtifact, Network, OutPoint, TreeStateArtifact, TxId, Zatoshis,
};
use zally_keys::{Mnemonic, SeedMaterial};
use zally_storage::{
    ScanRequest, ShieldTransparentRequest, Sqlite, SqliteOptions, StorageError, TransparentUtxoRow,
    WalletStorage,
};
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_client_backend::proto::service::TreeState;
use zcash_transparent::address::Script;

const ANCHOR_HEIGHT: u32 = 4_009_899;
const SCAN_START: u32 = 4_009_900;
const SCAN_END: u32 = 4_009_997;
const TRANSPARENT_MINED_HEIGHT: u32 = ANCHOR_HEIGHT - 1;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shielding_skips_excluded_outpoint_via_batched_gather() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let proving_parameters =
        sapling_proving_parameters::create_sapling_proving_parameters(temp.path())?;
    let storage = Sqlite::new(
        SqliteOptions::for_network(Network::Testnet, temp.path().join("wallet.db"))
            .with_sapling_proving_parameters(proving_parameters.spend, proving_parameters.output),
    );
    storage.open_or_create().await?;

    let mnemonic = Mnemonic::generate();
    let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
    let account_id = storage
        .create_account_for_seed(&seed, chain_state_at(ANCHOR_HEIGHT)?)
        .await?;
    let ua = storage
        .derive_next_address_with_transparent(account_id)
        .await?;
    let transparent = ua
        .transparent()
        .copied()
        .ok_or(TestError::TransparentReceiverMissing)?;
    let script_pub_key_bytes = Script::from(transparent.script()).0.0;

    storage
        .update_chain_tip(BlockHeight::from(SCAN_END))
        .await?;
    storage
        .scan_blocks(ScanRequest::new(
            load_blocks(u64::from(SCAN_START), u64::from(SCAN_END))?,
            BlockHeight::from(SCAN_START),
            chain_state_at(ANCHOR_HEIGHT)?,
        ))
        .await?;

    // The shielding policy requires 100 confirmations for transparent inputs and 3
    // confirmations for the shielded anchor. The synthetic transparent height leaves the
    // outputs mature at the target height while the shielded anchor lands on the latest
    // retained checkpoint in this scanned slice.
    let mined_height = BlockHeight::from(TRANSPARENT_MINED_HEIGHT);
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

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn chain_state_at(height: u32) -> Result<TreeStateArtifact, TestError> {
    let text = fs::read_to_string(fixtures_dir().join(format!("treestate_{height}.json")))?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let rpc_result = &json["result"];
    let tree_hex = |pool: &str| -> Result<String, TestError> {
        rpc_result[pool]["commitments"]["finalState"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| TestError::fixture(format!("missing {pool} finalState at {height}")))
    };
    let tree_state = TreeState {
        network: "test".to_owned(),
        height: rpc_result["height"]
            .as_u64()
            .ok_or_else(|| TestError::fixture(format!("missing height at {height}")))?,
        hash: rpc_result["hash"]
            .as_str()
            .ok_or_else(|| TestError::fixture(format!("missing hash at {height}")))?
            .to_owned(),
        time: u32::try_from(
            rpc_result["time"]
                .as_u64()
                .ok_or_else(|| TestError::fixture(format!("missing time at {height}")))?,
        )
        .map_err(|_| TestError::fixture(format!("time out of range at {height}")))?,
        sapling_tree: tree_hex("sapling")?,
        orchard_tree: tree_hex("orchard")?,
        ironwood_tree: String::new(),
    };
    Ok(scan_artifact::tree_state_from_upstream(
        Network::Testnet,
        tree_state,
    ))
}

fn load_blocks(from_height: u64, to_height: u64) -> Result<Vec<CompactBlockArtifact>, TestError> {
    let framed = fs::read(fixtures_dir().join("compact_blocks_4009900_4009999.bin"))?;
    let mut blocks = Vec::new();
    let mut at = 0_usize;
    while at < framed.len() {
        let header: [u8; 4] = framed
            .get(at..at + 4)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| TestError::fixture("truncated block frame header"))?;
        at += 4;
        let frame_len = usize::try_from(u32::from_be_bytes(header))
            .map_err(|_| TestError::fixture("block frame length overflows usize"))?;
        let body = framed
            .get(at..at + frame_len)
            .ok_or_else(|| TestError::fixture("truncated block frame body"))?;
        at += frame_len;
        let block = CompactBlock::decode(body)?;
        if (from_height..=to_height).contains(&block.height) {
            blocks.push(scan_artifact::compact_block_from_upstream(block));
        }
    }
    Ok(blocks)
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("fixture json error: {0}")]
    FixtureJson(#[from] serde_json::Error),
    #[error("fixture proto decode error: {0}")]
    FixtureProto(#[from] prost::DecodeError),
    #[error("fixture error: {0}")]
    Fixture(String),
    #[error("derived UA did not contain a transparent receiver")]
    TransparentReceiverMissing,
}

impl TestError {
    fn fixture(message: impl Into<String>) -> Self {
        Self::Fixture(message.into())
    }
}
