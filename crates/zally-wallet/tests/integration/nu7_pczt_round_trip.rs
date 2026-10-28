//! A PCZT created at an NU7 target height round-trips through Zally's PCZT roles and extracts
//! to a version 6 transaction on the NU7 consensus branch with the NU7 default expiry.
//!
//! The wallet runs on a synthetic Regtest chain where every upgrade through NU7 activates at
//! height 1. It shields a recorded transparent UTXO into an Ironwood note, observes that
//! transaction in a compact block, and then creates (storage `create_pczt`), proves
//! (`Prover`), signs (`Signer`), and extracts (storage `extract_and_store_pczt`, the
//! wallet's extraction path) a payment PCZT spending the note.

use orchard::tree::MerkleHashOrchard;
use tempfile::TempDir;
use zally_core::{
    BlockHash, BlockHeight, CompactBlockArtifact, CompactChainMetadata, CompactShieldedAction,
    CompactTransaction, CompactTransparentInput, Network, TreeStateArtifact, TxId, Zatoshis,
};
use zally_keys::SeedMaterial;
use zally_pczt::{PcztBytes, PcztError, Prover, Signer};
use zally_storage::{
    ProposalPaymentRequest, ScanRequest, ShieldTransparentRequest, Sqlite, SqliteOptions,
    StorageError, TransparentUtxoRow, WalletStorage,
};
use zcash_client_backend::proto::compact_formats::CompactOrchardAction;
use zcash_primitives::transaction::builder::default_tx_expiry_delta;
use zcash_primitives::transaction::{Transaction, TxVersion};
use zcash_protocol::consensus::BranchId;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_transparent::address::Script;

const EMPTY_CHAIN_TIP_HEIGHT: u32 = 110;
const NOTE_MINED_HEIGHT: u32 = EMPTY_CHAIN_TIP_HEIGHT + 1;
/// Last block of the first post-note scan batch, which leaves a checkpoint there. It becomes
/// the PCZT's anchor, at which the note has the ten confirmations an untrusted note needs.
const ANCHOR_HEIGHT: u32 = NOTE_MINED_HEIGHT + 7;
/// Chain tip when the PCZT is created; the wallet scans up to it so the note's shard has no
/// unscanned range.
const PCZT_CHAIN_TIP_HEIGHT: u32 = ANCHOR_HEIGHT + 4;
const TRANSPARENT_FUNDING_TX_ID: [u8; 32] = [0x77; 32];
const TRANSPARENT_UTXO_ZAT: u64 = 1_000_000;
const PAYMENT_ZAT: u64 = 100_000;
/// `z_gettreestate` encoding of an empty commitment-tree frontier.
const EMPTY_FRONTIER: [u8; 3] = [0, 0, 0];

fn regtest_with_nu7_from_height_one() -> Network {
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
    let mut bytes = [0x7a; 32];
    bytes[..4].copy_from_slice(&height.to_le_bytes());
    BlockHash::from_bytes(bytes)
}

fn block(
    height: u32,
    transactions: Vec<CompactTransaction>,
    ironwood_tree_size: u32,
) -> CompactBlockArtifact {
    CompactBlockArtifact {
        height: BlockHeight::from(height),
        block_hash: synthetic_hash(height),
        previous_block_hash: if height == 1 {
            BlockHash::from_bytes([0; 32])
        } else {
            synthetic_hash(height - 1)
        },
        block_time_seconds: height,
        transactions,
        chain_metadata: CompactChainMetadata {
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: ironwood_tree_size,
        },
    }
}

/// Tree state of the synthetic chain at `height`, while every tree is still empty.
fn empty_tree_state(network: Network, height: u32) -> TreeStateArtifact {
    let empty_frontier = if height == 0 {
        Vec::new()
    } else {
        EMPTY_FRONTIER.to_vec()
    };
    TreeStateArtifact {
        network,
        height: BlockHeight::from(height),
        block_hash: if height == 0 {
            BlockHash::from_bytes([0; 32])
        } else {
            synthetic_hash(height)
        },
        block_time_seconds: height,
        sapling_final_state_bytes: empty_frontier.clone(),
        orchard_final_state_bytes: empty_frontier.clone(),
        ironwood_final_state_bytes: empty_frontier,
    }
}

async fn open_wallet(
    network: Network,
    temp: &TempDir,
    seed: &SeedMaterial,
) -> Result<(Sqlite, zally_core::AccountId), TestError> {
    let (spend_bytes, output_bytes) = wagyu_zcash_parameters::load_sapling_parameters();
    let spend_path = temp.path().join(zcash_proofs::SAPLING_SPEND_NAME);
    let output_path = temp.path().join(zcash_proofs::SAPLING_OUTPUT_NAME);
    std::fs::write(&spend_path, spend_bytes)?;
    std::fs::write(&output_path, output_bytes)?;
    let storage = Sqlite::new(
        SqliteOptions::for_network(network, temp.path().join("wallet.db"))
            .with_sapling_proving_parameters(spend_path, output_path),
    );
    storage.open_or_create().await?;
    let account_id = storage
        .create_account_for_seed(seed, empty_tree_state(network, 0))
        .await?;
    storage
        .update_chain_tip(BlockHeight::from(EMPTY_CHAIN_TIP_HEIGHT))
        .await?;
    // The scanner checkpoints the last block of each batch; one block per batch leaves a
    // proposal anchor at every height.
    for height in 1..=EMPTY_CHAIN_TIP_HEIGHT {
        storage
            .scan_blocks(ScanRequest::new(
                vec![block(height, Vec::new(), 0)],
                BlockHeight::from(height),
                empty_tree_state(network, height - 1),
            ))
            .await?;
    }
    Ok((storage, account_id))
}

/// Shields a mature transparent UTXO and returns the parsed shielding transaction.
async fn shield_into_ironwood(
    storage: &Sqlite,
    account_id: zally_core::AccountId,
    seed: &SeedMaterial,
) -> Result<Transaction, TestError> {
    let address = storage
        .derive_next_address_with_transparent(account_id)
        .await?;
    let transparent = address
        .transparent()
        .copied()
        .ok_or_else(|| TestError::fixture("derived address has no transparent receiver"))?;
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
            ShieldTransparentRequest::new(account_id, zatoshis(1_000)?),
            seed,
        )
        .await?
        .pop()
        .ok_or_else(|| TestError::fixture("shielding returned no transaction"))?;
    Ok(Transaction::read(
        prepared.raw_bytes.as_slice(),
        BranchId::Nu7,
    )?)
}

/// The compact form of `transaction`'s Ironwood actions, as a chain source would serve it.
fn compact_shielding(transaction: &Transaction) -> Result<CompactTransaction, TestError> {
    let bundle = transaction
        .ironwood_bundle()
        .ok_or_else(|| TestError::fixture("post-NU6.3 shielding must use the Ironwood pool"))?;
    let ironwood_actions = bundle
        .actions()
        .iter()
        .map(|action| {
            let compact = CompactOrchardAction::from(action);
            Ok(CompactShieldedAction {
                nullifier_bytes: fixed_bytes(compact.nullifier)?,
                commitment_bytes: fixed_bytes(compact.cmx)?,
                ephemeral_key_bytes: fixed_bytes(compact.ephemeral_key)?,
                ciphertext_bytes: fixed_bytes(compact.ciphertext)?,
            })
        })
        .collect::<Result<Vec<_>, TestError>>()?;
    Ok(CompactTransaction {
        index: 1,
        tx_id: TxId::from_bytes(*transaction.txid().as_ref()),
        fee_zat: None,
        sapling_spends: Vec::new(),
        sapling_outputs: Vec::new(),
        orchard_actions: Vec::new(),
        ironwood_actions,
        transparent_inputs: vec![CompactTransparentInput {
            previous_tx_id: TxId::from_bytes(TRANSPARENT_FUNDING_TX_ID),
            previous_output_index: 0,
        }],
        transparent_outputs: Vec::new(),
    })
}

/// `z_gettreestate` encoding of the Ironwood frontier holding `transaction`'s commitments.
fn ironwood_frontier_bytes(transaction: &Transaction) -> Result<Vec<u8>, TestError> {
    let bundle = transaction
        .ironwood_bundle()
        .ok_or_else(|| TestError::fixture("post-NU6.3 shielding must use the Ironwood pool"))?;
    let mut tree =
        incrementalmerkletree::frontier::CommitmentTree::<MerkleHashOrchard, 32>::empty();
    for action in bundle.actions() {
        tree.append(MerkleHashOrchard::from_cmx(action.cmx()))
            .map_err(|()| TestError::fixture("synthetic Ironwood tree is full"))?;
    }
    let mut bytes = Vec::new();
    zcash_primitives::merkle_tree::write_commitment_tree(&tree, &mut bytes)?;
    Ok(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pczt_at_nu7_round_trips_through_prover_signer_and_extractor() -> Result<(), TestError> {
    let network = regtest_with_nu7_from_height_one();
    let temp = TempDir::new()?;
    let seed = SeedMaterial::from_raw_bytes(vec![0x5a; 32])
        .map_err(|err| TestError::fixture(err.to_string()))?;
    let (storage, account_id) = open_wallet(network, &temp, &seed).await?;

    let shielding = shield_into_ironwood(&storage, account_id, &seed).await?;
    let compact = compact_shielding(&shielding)?;
    let ironwood_tree_size = u32::try_from(compact.ironwood_actions.len())
        .map_err(|_| TestError::fixture("action count overflows u32"))?;
    storage
        .update_chain_tip(BlockHeight::from(PCZT_CHAIN_TIP_HEIGHT))
        .await?;
    let empty_blocks = |heights: std::ops::RangeInclusive<u32>| {
        heights.map(move |height| block(height, Vec::new(), ironwood_tree_size))
    };
    storage
        .scan_blocks(ScanRequest::new(
            std::iter::once(block(NOTE_MINED_HEIGHT, vec![compact], ironwood_tree_size))
                .chain(empty_blocks(NOTE_MINED_HEIGHT + 1..=ANCHOR_HEIGHT))
                .collect(),
            BlockHeight::from(NOTE_MINED_HEIGHT),
            empty_tree_state(network, EMPTY_CHAIN_TIP_HEIGHT),
        ))
        .await?;
    let mut anchor_state = empty_tree_state(network, ANCHOR_HEIGHT);
    anchor_state.ironwood_final_state_bytes = ironwood_frontier_bytes(&shielding)?;
    storage
        .scan_blocks(ScanRequest::new(
            empty_blocks(ANCHOR_HEIGHT + 1..=PCZT_CHAIN_TIP_HEIGHT).collect(),
            BlockHeight::from(ANCHOR_HEIGHT + 1),
            anchor_state,
        ))
        .await?;

    let recipient = storage
        .derive_next_address(account_id)
        .await?
        .encode(&network.to_parameters());
    let created = storage
        .create_pczt(
            ProposalPaymentRequest::new(account_id, recipient, zatoshis(PAYMENT_ZAT)?, None),
            None,
        )
        .await?;
    let proved = Prover::new(network)
        .prove_with_seed(PcztBytes::from_serialized(created, network), &seed)
        .await?;
    let signed = Signer::new(network).sign_with_seed(proved, &seed).await?;
    let extracted = storage.extract_and_store_pczt(signed.into_bytes()).await?;

    let transaction = Transaction::read(extracted.raw_bytes.as_slice(), BranchId::Nu7)?;
    let target_height = PCZT_CHAIN_TIP_HEIGHT + 1;
    assert_eq!(transaction.version(), TxVersion::V6);
    assert_eq!(transaction.consensus_branch_id(), BranchId::Nu7);
    assert_eq!(u32::from(transaction.consensus_branch_id()), 0x7719_0AD9);
    assert!(transaction.ironwood_bundle().is_some());
    assert_eq!(
        u32::from(transaction.expiry_height()),
        target_height + default_tx_expiry_delta(BranchId::Nu7)
    );
    Ok(())
}

fn zatoshis(zat: u64) -> Result<Zatoshis, TestError> {
    Zatoshis::try_from(zat).map_err(|err| TestError::fixture(err.to_string()))
}

fn fixed_bytes<const N: usize>(bytes: Vec<u8>) -> Result<[u8; N], TestError> {
    bytes
        .try_into()
        .map_err(|_| TestError::fixture("compact action field has the wrong length"))
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("PCZT error: {0}")]
    Pczt(#[from] PcztError),
    #[error("fixture error: {0}")]
    Fixture(String),
}

impl TestError {
    fn fixture(reason: impl Into<String>) -> Self {
        Self::Fixture(reason.into())
    }
}
