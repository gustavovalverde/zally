//! `WalletStorage::commitment_tree_roots` must report roots anchored at the latest scanned
//! checkpoint. Subtree-root backfill can insert the completed root of the subtree the wallet
//! birthday falls inside (the subtree is complete at the chain tip while the scan frontier is
//! still inside it). That root commits leaves beyond the scan frontier, so a root computed
//! over all tree nodes would report the subtree's final state and stay frozen while blocks
//! inside the subtree are scanned, faulting tree-root verification after every scan.
//!
//! Fixtures (`tests/fixtures/`): captured Zcash testnet compact blocks 4009900-4009999 (66
//! Orchard note commitments, no Sapling outputs) as length-framed (u32 big-endian)
//! `CompactBlock` protos, plus `z_gettreestate` responses at heights 4009899, 4009949, and
//! 4009999. The wallet anchors at 4009899, whose Orchard frontier (position 176046) lies
//! inside Orchard subtree index 2 (positions 131072-196607); that subtree is complete on
//! chain, so its finished root is a valid backfill input while this range is scanned.

use std::fs;
use std::path::{Path, PathBuf};

use prost::Message as _;
use tempfile::TempDir;
use zally_core::{BlockHeight, Network};
use zally_keys::{SeedMaterial, SeedMaterialError};
use zally_storage::{ScanRequest, Sqlite, SqliteOptions, StorageError, WalletStorage};
use zcash_client_backend::data_api::chain::ChainState;
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_client_backend::proto::service::TreeState;
use zcash_protocol::ShieldedPool;

const ANCHOR_HEIGHT: u32 = 4_009_899;
const RANGE_START: u64 = 4_009_900;
const RANGE_SPLIT: u32 = 4_009_949;
const RANGE_END: u32 = 4_009_999;

/// The chain's completed Orchard subtree roots for indices 0-2.
///
/// Entries are `(completing block height, root hex)` as `z_getsubtreesbyindex` reports them.
/// Index 2 contains this range's note commitments and completes above [`RANGE_END`].
const ORCHARD_SUBTREE_ROOTS: [(u32, &str); 3] = [
    (
        3_364_755,
        "25934a8c8cde7b4ba7e51d78f2321c7e286d140811a192f692f29d3f0ecce510",
    ),
    (
        3_861_020,
        "a7d4af61ae5f9c5a63dd74d7bb541f42ca227c79c30ed360c569167f7dedfc1e",
    ),
    (
        4_094_022,
        "78fccfdeba6dcd684fa879326e6822f59cd0a22c0aaa6df1e5596a877f870128",
    ),
];

/// A linear scan of the full range anchored at the birthday advances both roots to the
/// chain's tree-state roots at the scanned height.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linear_scan_tracks_chain_roots() -> Result<(), TestError> {
    let (storage, _temp) = fresh_wallet().await?;

    storage
        .scan_blocks(ScanRequest::new(
            load_blocks(RANGE_START, u64::from(RANGE_END))?,
            BlockHeight::from(
                u32::try_from(RANGE_START).map_err(|_| TestError::fixture("range start height"))?,
            ),
            chain_state_at(ANCHOR_HEIGHT)?,
        ))
        .await?;

    assert_roots_match_chain(&storage, RANGE_END).await
}

/// Scanning the upper half of the range first (anchored mid-range) and the birthday half
/// second still leaves both roots at the chain's roots for the highest scanned height.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tip_first_scan_tracks_chain_roots() -> Result<(), TestError> {
    let (storage, _temp) = fresh_wallet().await?;

    storage
        .update_chain_tip(BlockHeight::from(RANGE_END))
        .await?;
    storage
        .scan_blocks(ScanRequest::new(
            load_blocks(u64::from(RANGE_SPLIT) + 1, u64::from(RANGE_END))?,
            BlockHeight::from(RANGE_SPLIT + 1),
            chain_state_at(RANGE_SPLIT)?,
        ))
        .await?;
    assert_roots_match_chain(&storage, RANGE_END).await?;

    storage
        .scan_blocks(ScanRequest::new(
            load_blocks(RANGE_START, u64::from(RANGE_SPLIT))?,
            BlockHeight::from(
                u32::try_from(RANGE_START).map_err(|_| TestError::fixture("range start height"))?,
            ),
            chain_state_at(ANCHOR_HEIGHT)?,
        ))
        .await?;

    assert_roots_match_chain(&storage, RANGE_END).await
}

/// Backfilling the completed root of the subtree containing the wallet birthday must not
/// leak into the reported roots.
///
/// That root commits leaves beyond the scan frontier; after scanning the birthday range the
/// reported roots must still be checkpoint-anchored and match the chain at the scanned
/// height.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backfilled_birthday_subtree_keeps_checkpoint_roots() -> Result<(), TestError> {
    let (storage, _temp) = fresh_wallet().await?;

    storage
        .update_chain_tip(BlockHeight::from(RANGE_END))
        .await?;
    storage
        .put_subtree_roots(ShieldedPool::Orchard, 0, subtree_roots(3)?)
        .await?;

    storage
        .scan_blocks(ScanRequest::new(
            load_blocks(RANGE_START, u64::from(RANGE_END))?,
            BlockHeight::from(
                u32::try_from(RANGE_START).map_err(|_| TestError::fixture("range start height"))?,
            ),
            chain_state_at(ANCHOR_HEIGHT)?,
        ))
        .await?;

    assert_roots_match_chain(&storage, RANGE_END).await
}

/// Backfilling only the subtree roots that lie entirely below the birthday frontier does not
/// disturb the reported roots either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backfill_below_frontier_tracks_chain_roots() -> Result<(), TestError> {
    let (storage, _temp) = fresh_wallet().await?;

    storage
        .update_chain_tip(BlockHeight::from(RANGE_END))
        .await?;
    storage
        .put_subtree_roots(ShieldedPool::Orchard, 0, subtree_roots(2)?)
        .await?;

    storage
        .scan_blocks(ScanRequest::new(
            load_blocks(RANGE_START, u64::from(RANGE_END))?,
            BlockHeight::from(
                u32::try_from(RANGE_START).map_err(|_| TestError::fixture("range start height"))?,
            ),
            chain_state_at(ANCHOR_HEIGHT)?,
        ))
        .await?;

    assert_roots_match_chain(&storage, RANGE_END).await
}

async fn assert_roots_match_chain(storage: &Sqlite, height: u32) -> Result<(), TestError> {
    let wallet = storage.commitment_tree_roots().await?;
    let chain = chain_state_at(height)?;
    assert_eq!(
        wallet.sapling,
        Some(chain.final_sapling_tree().root().to_bytes()),
        "wallet sapling root at the latest checkpoint must match the chain root at {height}",
    );
    assert_eq!(
        wallet.orchard,
        Some(chain.final_orchard_tree().root().to_bytes()),
        "wallet orchard root at the latest checkpoint must match the chain root at {height}",
    );
    Ok(())
}

async fn fresh_wallet() -> Result<(Sqlite, TempDir), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        Network::Testnet,
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;
    let seed = SeedMaterial::from_raw_bytes(vec![7u8; 32])?;
    storage
        .create_account_for_seed(&seed, chain_state_at(ANCHOR_HEIGHT)?)
        .await?;
    Ok((storage, temp))
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn chain_state_at(height: u32) -> Result<ChainState, TestError> {
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
    Ok(tree_state.to_chain_state()?)
}

fn load_blocks(from_height: u64, to_height: u64) -> Result<Vec<CompactBlock>, TestError> {
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
            blocks.push(block);
        }
    }
    Ok(blocks)
}

fn subtree_roots(count: usize) -> Result<Vec<(BlockHeight, [u8; 32])>, TestError> {
    ORCHARD_SUBTREE_ROOTS
        .iter()
        .take(count)
        .map(|(completing_height, root_hex)| {
            let root: [u8; 32] = hex::decode(root_hex)?
                .try_into()
                .map_err(|_| TestError::fixture("subtree root is not 32 bytes"))?;
            Ok((BlockHeight::from(*completing_height), root))
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("seed error: {0}")]
    Seed(#[from] SeedMaterialError),
    #[error("fixture json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("fixture proto error: {0}")]
    Proto(#[from] prost::DecodeError),
    #[error("fixture hex error: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("fixture error: {0}")]
    Fixture(String),
}

impl TestError {
    fn fixture(reason: impl Into<String>) -> Self {
        Self::Fixture(reason.into())
    }
}
