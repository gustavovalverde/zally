//! `WalletStorage::revert_blocks_above` rewinds the wallet to a fork point and, in the same
//! database transaction, records the wallet-sent transactions the rewind un-mined and the
//! chain-event cursor that announced the reorg.
//!
//! Each case runs a Regtest wallet over a synthetic chain. The wallet shields a recorded
//! transparent UTXO to Sapling, so the chain holds a transaction the wallet sent and a note it
//! received, both mined in one block above the fork point.

use tempfile::TempDir;
use zally_core::{
    BlockHash, BlockHeight, CompactBlockArtifact, CompactChainMetadata, CompactSaplingOutput,
    CompactTransaction, CompactTransparentInput, Network, TreeStateArtifact, TxId, Zatoshis,
};
use zally_keys::{SeedMaterial, SeedMaterialError};
use zally_storage::{
    ScanRequest, ShieldTransparentRequest, Sqlite, SqliteOptions, StorageError, TransparentUtxoRow,
    WalletStorage,
};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::ShieldedPool;
use zcash_protocol::consensus::BranchId;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_transparent::address::Script;

#[path = "fixtures/sapling_proving_parameters.rs"]
mod sapling_proving_parameters;

const FUNDED_TIP_HEIGHT: u32 = 110;
const SHIELDING_MINED_HEIGHT: u32 = FUNDED_TIP_HEIGHT + 1;
const CHAIN_TIP_HEIGHT: u32 = SHIELDING_MINED_HEIGHT + 9;
const FORK_POINT_HEIGHT: u32 = FUNDED_TIP_HEIGHT - 5;
const TRANSPARENT_FUNDING_TX_ID: [u8; 32] = [0x77; 32];
const TRANSPARENT_UTXO_ZAT: u64 = 1_000_000;
const EMPTY_FRONTIER: [u8; 3] = [0, 0, 0];

fn regtest_from_height_one() -> Network {
    let height_one = Some(zcash_protocol::consensus::BlockHeight::from_u32(1));
    Network::Regtest(LocalNetwork {
        overwinter: height_one,
        sapling: height_one,
        blossom: height_one,
        heartwood: height_one,
        canopy: height_one,
        nu5: height_one,
        nu6: height_one,
        nu6_1: height_one,
        nu6_2: height_one,
        nu6_3: height_one,
        nu7: height_one,
    })
}

fn synthetic_hash(height: u32) -> BlockHash {
    if height == 0 {
        return BlockHash::from_bytes([0; 32]);
    }
    let mut bytes = [0x7a; 32];
    bytes[..4].copy_from_slice(&height.to_le_bytes());
    BlockHash::from_bytes(bytes)
}

fn block(
    height: u32,
    transactions: Vec<CompactTransaction>,
    sapling_size: u32,
) -> CompactBlockArtifact {
    CompactBlockArtifact {
        height: BlockHeight::from(height),
        block_hash: synthetic_hash(height),
        previous_block_hash: synthetic_hash(height - 1),
        block_time_seconds: height,
        transactions,
        chain_metadata: CompactChainMetadata {
            sapling_commitment_tree_size: sapling_size,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        },
    }
}

fn tree_state(network: Network, height: u32, sapling_frontier: &[u8]) -> TreeStateArtifact {
    let empty = if height == 0 {
        Vec::new()
    } else {
        EMPTY_FRONTIER.to_vec()
    };
    TreeStateArtifact {
        network,
        height: BlockHeight::from(height),
        block_hash: synthetic_hash(height),
        block_time_seconds: height,
        sapling_final_state_bytes: if sapling_frontier.is_empty() {
            empty.clone()
        } else {
            sapling_frontier.to_vec()
        },
        orchard_final_state_bytes: empty.clone(),
        ironwood_final_state_bytes: empty,
    }
}

struct FundedWallet {
    storage: Sqlite,
    account_id: zally_core::AccountId,
    shielding: Transaction,
    sapling_frontier: Vec<u8>,
    network: Network,
    _temp: TempDir,
}

/// A wallet that shielded its transparent UTXO and scanned the shielding transaction mined
/// at [`SHIELDING_MINED_HEIGHT`], up to [`CHAIN_TIP_HEIGHT`].
async fn funded_wallet() -> Result<FundedWallet, TestError> {
    let network = regtest_from_height_one();
    let temp = TempDir::new()?;
    let proving_parameters =
        sapling_proving_parameters::create_sapling_proving_parameters(temp.path())?;
    let storage = Sqlite::new(
        SqliteOptions::for_network(network, temp.path().join("wallet.db"))
            .with_sapling_proving_parameters(proving_parameters.spend, proving_parameters.output),
    );
    storage.open_or_create().await?;
    let seed = SeedMaterial::from_raw_bytes(vec![0x5a; 32])?;
    let account_id = storage
        .create_account_for_seed(&seed, tree_state(network, 0, &[]))
        .await?;
    storage
        .update_chain_tip(BlockHeight::from(FUNDED_TIP_HEIGHT))
        .await?;
    for chunk_start in (1..=FUNDED_TIP_HEIGHT).step_by(10) {
        let chunk_end = (chunk_start + 9).min(FUNDED_TIP_HEIGHT);
        storage
            .scan_blocks(ScanRequest::new(
                (chunk_start..=chunk_end)
                    .map(|height| block(height, Vec::new(), 0))
                    .collect(),
                BlockHeight::from(chunk_start),
                tree_state(network, chunk_start - 1, &[]),
            ))
            .await?;
    }

    let address = storage
        .derive_next_address_with_transparent(account_id)
        .await?;
    let transparent = address.transparent().copied().ok_or(TestError::Fixture(
        "derived address has no transparent receiver",
    ))?;
    storage
        .record_transparent_utxos(
            None,
            vec![TransparentUtxoRow::new(
                TxId::from_bytes(TRANSPARENT_FUNDING_TX_ID),
                0,
                zatoshis(TRANSPARENT_UTXO_ZAT)?,
                BlockHeight::from(1),
                Script::from(transparent.script()).0.0,
            )],
        )
        .await?;
    let prepared = storage
        .shield_transparent_funds(
            ShieldTransparentRequest::new(account_id, zatoshis(1_000)?)
                .with_destination_pool(ShieldedPool::Sapling),
            &seed,
        )
        .await?
        .pop()
        .ok_or(TestError::Fixture("shielding returned no transaction"))?;
    let shielding = Transaction::read(prepared.raw_bytes.as_slice(), BranchId::Nu7)?;
    let sapling_frontier = sapling_frontier_bytes(&shielding)?;

    storage
        .update_chain_tip(BlockHeight::from(CHAIN_TIP_HEIGHT))
        .await?;
    scan_shielding_branch(
        &storage,
        network,
        &shielding,
        &sapling_frontier,
        FORK_POINT_HEIGHT,
    )
    .await?;
    Ok(FundedWallet {
        storage,
        account_id,
        shielding,
        sapling_frontier,
        network,
        _temp: temp,
    })
}

/// Scans `(from_height, CHAIN_TIP_HEIGHT]` on the branch that mines `shielding` at
/// [`SHIELDING_MINED_HEIGHT`].
async fn scan_shielding_branch(
    storage: &Sqlite,
    network: Network,
    shielding: &Transaction,
    sapling_frontier: &[u8],
    from_height: u32,
) -> Result<(), TestError> {
    let sapling_size = sapling_output_count(shielding)?;
    let blocks: Vec<CompactBlockArtifact> = (from_height + 1..=CHAIN_TIP_HEIGHT)
        .map(|height| match height.cmp(&SHIELDING_MINED_HEIGHT) {
            std::cmp::Ordering::Less => Ok(block(height, Vec::new(), 0)),
            std::cmp::Ordering::Equal => Ok(block(
                height,
                vec![compact_shielding(shielding)?],
                sapling_size,
            )),
            std::cmp::Ordering::Greater => Ok(block(height, Vec::new(), sapling_size)),
        })
        .collect::<Result<_, TestError>>()?;
    let anchor_frontier: &[u8] = if from_height >= SHIELDING_MINED_HEIGHT {
        sapling_frontier
    } else {
        &[]
    };
    storage
        .scan_blocks(ScanRequest::new(
            blocks,
            BlockHeight::from(from_height + 1),
            tree_state(network, from_height, anchor_frontier),
        ))
        .await?;
    Ok(())
}

fn sapling_output_count(transaction: &Transaction) -> Result<u32, TestError> {
    let count = transaction
        .sapling_bundle()
        .map_or(0, |bundle| bundle.shielded_outputs().len());
    u32::try_from(count).map_err(|_| TestError::Fixture("sapling output count overflows u32"))
}

fn compact_shielding(transaction: &Transaction) -> Result<CompactTransaction, TestError> {
    let bundle = transaction.sapling_bundle().ok_or(TestError::Fixture(
        "shielding to Sapling must carry a Sapling bundle",
    ))?;
    let sapling_outputs = bundle
        .shielded_outputs()
        .iter()
        .map(|output| {
            let ciphertext: [u8; 52] = output.enc_ciphertext()[..52]
                .try_into()
                .map_err(|_| TestError::Fixture("Sapling ciphertext is shorter than 52 bytes"))?;
            Ok(CompactSaplingOutput {
                commitment_bytes: output.cmu().to_bytes(),
                ephemeral_key_bytes: output.ephemeral_key().0,
                ciphertext_bytes: ciphertext,
            })
        })
        .collect::<Result<Vec<_>, TestError>>()?;
    Ok(CompactTransaction {
        index: 1,
        tx_id: TxId::from_bytes(*transaction.txid().as_ref()),
        fee_zat: None,
        sapling_spends: Vec::new(),
        sapling_outputs,
        orchard_actions: Vec::new(),
        ironwood_actions: Vec::new(),
        transparent_inputs: vec![CompactTransparentInput {
            previous_tx_id: TxId::from_bytes(TRANSPARENT_FUNDING_TX_ID),
            previous_output_index: 0,
        }],
        transparent_outputs: Vec::new(),
    })
}

fn sapling_frontier_bytes(transaction: &Transaction) -> Result<Vec<u8>, TestError> {
    let bundle = transaction.sapling_bundle().ok_or(TestError::Fixture(
        "shielding to Sapling must carry a Sapling bundle",
    ))?;
    let mut tree = incrementalmerkletree::frontier::CommitmentTree::<sapling::Node, 32>::empty();
    for output in bundle.shielded_outputs() {
        tree.append(sapling::Node::from_cmu(output.cmu()))
            .map_err(|()| TestError::Fixture("synthetic Sapling tree is full"))?;
    }
    let mut bytes = Vec::new();
    zcash_primitives::merkle_tree::write_commitment_tree(&tree, &mut bytes)?;
    Ok(bytes)
}

fn zatoshis(zat: u64) -> Result<Zatoshis, TestError> {
    Zatoshis::try_from(zat).map_err(|_| TestError::Fixture("zatoshi value out of range"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewind_records_reverted_sends_and_the_cursor_with_the_truncation() -> Result<(), TestError>
{
    let wallet = funded_wallet().await?;
    let shielding_tx_id = TxId::from_bytes(*wallet.shielding.txid().as_ref());
    assert_eq!(
        wallet.storage.find_highest_scanned_height().await?,
        Some(BlockHeight::from(CHAIN_TIP_HEIGHT))
    );
    assert_eq!(
        wallet
            .storage
            .find_scanned_block_hash(BlockHeight::from(SHIELDING_MINED_HEIGHT))
            .await?,
        Some(synthetic_hash(SHIELDING_MINED_HEIGHT))
    );

    let reverted = wallet
        .storage
        .revert_blocks_above(
            tree_state(wallet.network, FORK_POINT_HEIGHT, &[]),
            Some(b"cursor-after-the-reorg".to_vec()),
        )
        .await?;

    assert_eq!(
        reverted.transactions,
        vec![(shielding_tx_id, BlockHeight::from(SHIELDING_MINED_HEIGHT))]
    );
    assert_eq!(reverted.receives.len(), 1, "{:?}", reverted.receives);
    assert_eq!(reverted.receives[0].tx_id, shielding_tx_id);
    assert_eq!(
        reverted.receives[0].mined_height,
        BlockHeight::from(SHIELDING_MINED_HEIGHT)
    );
    assert_eq!(
        wallet.storage.find_highest_scanned_height().await?,
        Some(BlockHeight::from(FORK_POINT_HEIGHT))
    );
    assert_eq!(
        wallet.storage.find_chain_event_cursor().await?,
        Some(b"cursor-after-the-reorg".to_vec())
    );
    assert!(
        wallet
            .storage
            .list_shielded_receives_for_account(wallet.account_id)
            .await?
            .is_empty(),
        "an un-mined receive must leave the durable receive history"
    );

    let reverted_broadcasts = wallet.storage.list_reverted_broadcasts().await?;
    assert_eq!(reverted_broadcasts.len(), 1);
    let reverted_send = &reverted_broadcasts[0];
    assert_eq!(reverted_send.tx_id, shielding_tx_id);
    assert_eq!(
        reverted_send.reverted_from_height,
        Some(BlockHeight::from(SHIELDING_MINED_HEIGHT))
    );
    assert_eq!(
        reverted_send.expiry_height,
        BlockHeight::from(u32::from(wallet.shielding.expiry_height()))
    );
    assert_eq!(reverted_send.mined_height, None);
    assert_eq!(reverted_send.resubmitted_at_ms, None);
    let mut raw_tx_bytes = Vec::new();
    wallet.shielding.write(&mut raw_tx_bytes)?;
    assert_eq!(reverted_send.raw_tx_bytes, raw_tx_bytes);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reverted_send_reports_its_new_mined_height_once_scanned() -> Result<(), TestError> {
    let wallet = funded_wallet().await?;
    let shielding_tx_id = TxId::from_bytes(*wallet.shielding.txid().as_ref());
    wallet
        .storage
        .revert_blocks_above(tree_state(wallet.network, FORK_POINT_HEIGHT, &[]), None)
        .await?;

    wallet
        .storage
        .record_reverted_broadcast_resubmitted(shielding_tx_id, 42)
        .await?;
    scan_shielding_branch(
        &wallet.storage,
        wallet.network,
        &wallet.shielding,
        &wallet.sapling_frontier,
        FORK_POINT_HEIGHT,
    )
    .await?;
    let remined = wallet.storage.list_reverted_broadcasts().await?;
    assert_eq!(remined.len(), 1);
    assert_eq!(
        remined[0].mined_height,
        Some(BlockHeight::from(SHIELDING_MINED_HEIGHT)),
        "the row must report the transaction mined again once the wallet scans it"
    );
    assert_eq!(remined[0].resubmitted_at_ms, Some(42));

    wallet
        .storage
        .clear_reverted_broadcast(shielding_tx_id)
        .await?;
    assert!(wallet.storage.list_reverted_broadcasts().await?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reverted_sends_survive_a_rebuild() -> Result<(), TestError> {
    let wallet = funded_wallet().await?;
    let shielding_tx_id = TxId::from_bytes(*wallet.shielding.txid().as_ref());
    wallet
        .storage
        .revert_blocks_above(tree_state(wallet.network, FORK_POINT_HEIGHT, &[]), None)
        .await?;
    assert_eq!(wallet.storage.find_chain_event_cursor().await?, None);

    let seed = SeedMaterial::from_raw_bytes(vec![0x5a; 32])?;
    let rebuilt_id = wallet
        .storage
        .recreate_with_account(&seed, tree_state(wallet.network, 0, &[]))
        .await?;
    assert_eq!(rebuilt_id, wallet.account_id);

    let reverted_broadcasts = wallet.storage.list_reverted_broadcasts().await?;
    assert_eq!(reverted_broadcasts.len(), 1);
    assert_eq!(reverted_broadcasts[0].tx_id, shielding_tx_id);
    assert_eq!(
        reverted_broadcasts[0].mined_height, None,
        "the rebuilt wallet has not scanned the transaction yet"
    );
    Ok(())
}

#[tokio::test]
async fn chain_event_cursor_round_trips_and_survives_reopen() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let options = SqliteOptions::for_network(Network::regtest(), temp.path().join("wallet.db"));
    let storage = Sqlite::new(options.clone());
    storage.open_or_create().await?;
    assert_eq!(storage.find_chain_event_cursor().await?, None);

    storage.record_chain_event_cursor(vec![1, 2, 3]).await?;
    storage.record_chain_event_cursor(vec![4, 5, 6]).await?;
    assert_eq!(
        storage.find_chain_event_cursor().await?,
        Some(vec![4, 5, 6])
    );
    drop(storage);

    let reopened = Sqlite::new(options);
    reopened.open_or_create().await?;
    assert_eq!(
        reopened.find_chain_event_cursor().await?,
        Some(vec![4, 5, 6]),
        "the cursor must survive a process restart"
    );
    reopened.clear_chain_event_cursor().await?;
    assert_eq!(reopened.find_chain_event_cursor().await?, None);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("seed error: {0}")]
    Seed(#[from] SeedMaterialError),
    #[error("fixture error: {0}")]
    Fixture(&'static str),
}
