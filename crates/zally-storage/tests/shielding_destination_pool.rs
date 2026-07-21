//! Storage-level transparent shielding destination selection.
//!
//! The fixture scans a slice of the vendored testnet compact blocks before recording a
//! mature transparent UTXO. This gives the real `SQLite` backend enough chain state to build and
//! prove a shielding transaction through the public [`WalletStorage`] boundary.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use prost::Message as _;
use tempfile::TempDir;
#[path = "fixtures/sapling_proving_parameters.rs"]
mod sapling_proving_parameters;
#[path = "fixtures/scan_artifact.rs"]
mod scan_artifact;

use zally_core::{BlockHeight, CompactBlockArtifact, Network, TreeStateArtifact, TxId, Zatoshis};
use zally_keys::{Mnemonic, SeedMaterial};
use zally_storage::{
    PreparedTransaction, ScanRequest, ShieldTransparentRequest, Sqlite, SqliteOptions,
    StorageError, TransparentUtxoRow, WalletStorage,
};
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_client_backend::proto::service::TreeState;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::ShieldedPool;
use zcash_protocol::consensus::BranchId;
use zcash_transparent::address::Script;

const ANCHOR_HEIGHT: u32 = 4_009_899;
const SCAN_START: u32 = 4_009_900;
const SCAN_END: u32 = 4_009_997;
const TRANSPARENT_MINED_HEIGHT: u32 = ANCHOR_HEIGHT - 1;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shielding_can_select_sapling_destination() -> Result<(), TestError> {
    let prepared = prepare_shielding(Some(ShieldedPool::Sapling)).await?;
    let transaction = parse_prepared_transaction(&prepared)?;

    assert!(
        transaction.sapling_bundle().is_some(),
        "explicit Sapling destination must produce a Sapling bundle"
    );
    assert!(
        transaction.orchard_bundle().is_none(),
        "explicit Sapling destination must not fall back to Orchard"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shielding_without_selection_keeps_activation_default() -> Result<(), TestError> {
    let prepared = prepare_shielding(None).await?;
    let transaction = parse_prepared_transaction(&prepared)?;

    assert!(
        transaction.orchard_bundle().is_some(),
        "the pre-NU6.3 testnet fixture must retain the Orchard fallback"
    );
    assert!(
        transaction.sapling_bundle().is_none(),
        "default shielding must not change to Sapling"
    );
    Ok(())
}

async fn prepare_shielding(
    destination_pool: Option<ShieldedPool>,
) -> Result<PreparedTransaction, TestError> {
    let temp = TempDir::new()?;
    let network = Network::Testnet;
    let proving_parameters =
        sapling_proving_parameters::create_sapling_proving_parameters(temp.path())?;
    let storage = Sqlite::new(
        SqliteOptions::for_network(network, temp.path().join("wallet.db"))
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
    storage
        .record_transparent_utxos(vec![TransparentUtxoRow::new(
            TxId::from_bytes([0x33_u8; 32]),
            0,
            Zatoshis::try_from(1_000_000_u64).unwrap_or(Zatoshis::zero()),
            BlockHeight::from(TRANSPARENT_MINED_HEIGHT),
            script_pub_key_bytes,
        )])
        .await?;

    let threshold_zat = Zatoshis::try_from(1_000_u64).unwrap_or(Zatoshis::zero());
    let mut request = ShieldTransparentRequest::new(account_id, threshold_zat);
    if let Some(destination_pool) = destination_pool {
        request = request.with_destination_pool(destination_pool);
    }
    let mut prepared = storage
        .shield_transparent_funds(request, HashSet::new(), &seed)
        .await?;
    prepared
        .pop()
        .ok_or_else(|| TestError::fixture("shielding returned no transaction"))
}

fn parse_prepared_transaction(prepared: &PreparedTransaction) -> Result<Transaction, TestError> {
    let params = Network::Testnet.to_parameters();
    let branch_id = BranchId::for_height(
        &params,
        zcash_protocol::consensus::BlockHeight::from_u32(SCAN_END + 1),
    );
    Ok(Transaction::read(prepared.raw_bytes.as_slice(), branch_id)?)
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
