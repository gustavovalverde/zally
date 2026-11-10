//! Transactions the wallet builds at an NU7 target height use the NU7 consensus rules.
//!
//! Each case runs a Regtest wallet over a synthetic chain of empty blocks, records a mature
//! transparent UTXO, and shields it to Sapling through the public [`WalletStorage`]
//! boundary. Every upgrade through NU6.3 activates at height 1; only the NU7 activation
//! height varies. The consensus branch, transaction version, and default expiry delta all
//! come from the librustzcash cohort, not from Zally constants.

use tempfile::TempDir;
use zally_core::{
    BlockHash, BlockHeight, CompactBlockArtifact, CompactChainMetadata, Network, TxId, Zatoshis,
};
use zally_keys::{SeedMaterial, SeedMaterialError};
use zally_storage::{
    ScanRequest, ShieldTransparentRequest, Sqlite, SqliteOptions, StorageError, TransparentUtxoRow,
    WalletStorage,
};
use zcash_primitives::transaction::builder::default_tx_expiry_delta;
use zcash_primitives::transaction::{Transaction, TxVersion};
use zcash_protocol::ShieldedPool;
use zcash_protocol::consensus::BranchId;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_transparent::address::Script;

#[path = "fixtures/sapling_proving_parameters.rs"]
mod sapling_proving_parameters;
#[path = "fixtures/scan_artifact.rs"]
mod scan_artifact;

/// Chain tip of the synthetic chain; transactions target the next block.
const CHAIN_TIP_HEIGHT: u32 = 110;
const TARGET_HEIGHT: u32 = CHAIN_TIP_HEIGHT + 1;
/// Mined at height 1, so it is past the 100-block coinbase-safe shielding depth at the tip.
const TRANSPARENT_MINED_HEIGHT: u32 = 1;
const TRANSPARENT_UTXO_ZAT: u64 = 1_000_000;
/// `z_gettreestate` encoding of an empty commitment-tree frontier, required once a pool is
/// active.
const EMPTY_FRONTIER: [u8; 3] = [0, 0, 0];

fn regtest_with_nu7_at(nu7_activation_height: u32) -> Network {
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
        nu7: Some(zcash_protocol::consensus::BlockHeight::from_u32(
            nu7_activation_height,
        )),
    })
}

fn synthetic_hash(height: u32) -> BlockHash {
    let mut bytes = [0x7a; 32];
    bytes[..4].copy_from_slice(&height.to_le_bytes());
    BlockHash::from_bytes(bytes)
}

/// Empty blocks 1..=`tip`, chained from the all-zero genesis hash.
fn empty_chain(tip: u32) -> Vec<CompactBlockArtifact> {
    (1..=tip)
        .map(|height| CompactBlockArtifact {
            height: BlockHeight::from(height),
            block_hash: synthetic_hash(height),
            previous_block_hash: if height == 1 {
                BlockHash::from_bytes([0; 32])
            } else {
                synthetic_hash(height - 1)
            },
            block_time_seconds: height,
            transactions: Vec::new(),
            chain_metadata: CompactChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
                ironwood_commitment_tree_size: 0,
            },
        })
        .collect()
}

/// Builds a shielding transaction targeting [`TARGET_HEIGHT`] and returns it parsed.
async fn shield_at_target(network: Network) -> Result<Transaction, TestError> {
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
        .create_account_for_seed(
            &seed,
            scan_artifact::genesis_tree_state(network, 0, [0; 32]),
        )
        .await?;
    storage
        .update_chain_tip(BlockHeight::from(CHAIN_TIP_HEIGHT))
        .await?;
    // The scanner checkpoints the last block of each batch; scanning one block per batch
    // leaves a checkpoint, and so a proposal anchor, at every height.
    for block in empty_chain(CHAIN_TIP_HEIGHT) {
        let parent_height = block.height.as_u32() - 1;
        let mut parent = scan_artifact::genesis_tree_state(network, parent_height, [0; 32]);
        parent.block_hash = block.previous_block_hash;
        if parent_height > 0 {
            parent.sapling_final_state_bytes = EMPTY_FRONTIER.to_vec();
            parent.orchard_final_state_bytes = EMPTY_FRONTIER.to_vec();
            parent.ironwood_final_state_bytes = EMPTY_FRONTIER.to_vec();
        }
        storage
            .scan_blocks(ScanRequest::new(vec![block.clone()], block.height, parent))
            .await?;
    }

    let address = storage
        .derive_next_address_with_transparent(account_id)
        .await?;
    let transparent = address
        .transparent()
        .copied()
        .ok_or(TestError::TransparentReceiverMissing)?;
    storage
        .record_transparent_utxos(
            None,
            vec![TransparentUtxoRow::new(
                TxId::from_bytes([0x77; 32]),
                0,
                Zatoshis::try_from(TRANSPARENT_UTXO_ZAT).map_err(|_| TestError::Fixture)?,
                BlockHeight::from(TRANSPARENT_MINED_HEIGHT),
                Script::from(transparent.script()).0.0,
            )],
        )
        .await?;

    let request = ShieldTransparentRequest::new(
        account_id,
        Zatoshis::try_from(1_000_u64).map_err(|_| TestError::Fixture)?,
    )
    .with_destination_pool(ShieldedPool::Sapling);
    let prepared = storage
        .shield_transparent_funds(request, &seed)
        .await?
        .pop()
        .ok_or(TestError::NoTransaction)?;
    Ok(Transaction::read(
        prepared.raw_bytes.as_slice(),
        BranchId::Nu7,
    )?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transaction_at_nu7_activation_uses_nu7_rules() -> Result<(), TestError> {
    let transaction = shield_at_target(regtest_with_nu7_at(TARGET_HEIGHT)).await?;

    assert_eq!(transaction.version(), TxVersion::V6);
    assert_eq!(transaction.consensus_branch_id(), BranchId::Nu7);
    assert_eq!(u32::from(transaction.consensus_branch_id()), 0x7719_0AD9);
    assert_eq!(default_tx_expiry_delta(BranchId::Nu7), 120);
    assert_eq!(
        u32::from(transaction.expiry_height()),
        TARGET_HEIGHT + default_tx_expiry_delta(BranchId::Nu7)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transaction_before_nu7_activation_keeps_nu6_3_rules() -> Result<(), TestError> {
    let transaction = shield_at_target(regtest_with_nu7_at(TARGET_HEIGHT + 1)).await?;

    assert_eq!(transaction.version(), TxVersion::V6);
    assert_eq!(transaction.consensus_branch_id(), BranchId::Nu6_3);
    assert_eq!(default_tx_expiry_delta(BranchId::Nu6_3), 40);
    assert_eq!(
        u32::from(transaction.expiry_height()),
        TARGET_HEIGHT + default_tx_expiry_delta(BranchId::Nu6_3)
    );
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
    #[error("derived address has no transparent receiver")]
    TransparentReceiverMissing,
    #[error("shielding returned no transaction")]
    NoTransaction,
    #[error("fixture constant is out of range")]
    Fixture,
}
