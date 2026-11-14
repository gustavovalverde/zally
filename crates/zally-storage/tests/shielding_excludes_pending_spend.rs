//! Regression tests for the transparent inputs the shielding path may select.
//!
//! `create_proposed_transactions` stores the transaction before returning, and
//! librustzcash excludes outputs spent by that unmined transaction from subsequent
//! proposals. Zally must preserve that upstream lifecycle without a parallel
//! pending-outpoint selector. An output a receiver's complete unspent set no longer
//! contains is retired, so no proposal selects it, while one a pending shielding spends
//! keeps that spend.
//!
//! The wallet must scan at least one block before any proposal can be built: proposals
//! anchor at a note commitment tree checkpoint, and checkpoints exist only for scanned
//! blocks. The fixture scans a slice of the vendored testnet compact blocks (see
//! `commitment_tree_roots_regress.rs` for the fixture provenance) before shielding.

use std::fs;
use std::path::{Path, PathBuf};

use prost::Message as _;
use tempfile::TempDir;
#[path = "fixtures/sapling_proving_parameters.rs"]
mod sapling_proving_parameters;
#[path = "fixtures/scan_artifact.rs"]
mod scan_artifact;

use zally_core::{
    AccountId, BlockHeight, CompactBlockArtifact, Network, OutPoint, TreeStateArtifact, TxId,
    Zatoshis,
};
use zally_keys::{Mnemonic, SeedMaterial};
use zally_storage::{
    ProposalRejectionKind, ScanRequest, ShieldTransparentRequest, Sqlite, SqliteOptions,
    StorageError, TransparentUnspentSet, TransparentUnspentSetRecorded, TransparentUtxoRow,
    WalletStorage,
};
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_client_backend::proto::service::TreeState;
use zcash_transparent::address::Script;

const ANCHOR_HEIGHT: u32 = 4_009_899;
const SCAN_START: u32 = 4_009_900;
const SCAN_END: u32 = 4_009_997;
const TRANSPARENT_MINED_HEIGHT: u32 = ANCHOR_HEIGHT - 1;

/// A Testnet wallet scanned through [`SCAN_END`], with one transparent receiver and the
/// proving parameters a shielding needs.
struct ShieldingWallet {
    _temp: TempDir,
    storage: Sqlite,
    seed: SeedMaterial,
    account_id: AccountId,
    script_pub_key_bytes: Vec<u8>,
}

impl ShieldingWallet {
    async fn scanned() -> Result<Self, TestError> {
        let temp = TempDir::new()?;
        let proving_parameters =
            sapling_proving_parameters::create_sapling_proving_parameters(temp.path())?;
        let storage = Sqlite::new(
            SqliteOptions::for_network(Network::Testnet, temp.path().join("wallet.db"))
                .with_sapling_proving_parameters(
                    proving_parameters.spend,
                    proving_parameters.output,
                ),
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
        Ok(Self {
            _temp: temp,
            storage,
            seed,
            account_id,
            script_pub_key_bytes,
        })
    }

    /// A transparent output at the wallet's receiver, mined at [`TRANSPARENT_MINED_HEIGHT`].
    ///
    /// The shielding policy requires 100 confirmations for transparent inputs and 3
    /// confirmations for the shielded anchor. The synthetic transparent height leaves the
    /// output mature at the target height while the shielded anchor lands on the latest
    /// retained checkpoint in the scanned slice.
    fn utxo(&self, tx_id_byte: u8) -> TransparentUtxoRow {
        TransparentUtxoRow::new(
            TxId::from_bytes([tx_id_byte; 32]),
            0,
            Zatoshis::try_from(1_000_000_u64).unwrap_or(Zatoshis::zero()),
            BlockHeight::from(TRANSPARENT_MINED_HEIGHT),
            self.script_pub_key_bytes.clone(),
        )
    }

    /// Records `utxos` as the receiver's complete unspent set at [`SCAN_END`].
    async fn record_unspent_set(
        &self,
        utxos: Vec<TransparentUtxoRow>,
    ) -> Result<TransparentUnspentSetRecorded, TestError> {
        Ok(self
            .storage
            .record_transparent_unspent_set(
                None,
                TransparentUnspentSet::new(
                    Network::Testnet,
                    self.script_pub_key_bytes.clone(),
                    BlockHeight::from(SCAN_END),
                    utxos,
                ),
            )
            .await?)
    }

    async fn shield(&self) -> Result<Vec<OutPoint>, StorageError> {
        let threshold = Zatoshis::try_from(1_000_u64).unwrap_or(Zatoshis::zero());
        let prepared = self
            .storage
            .shield_transparent_funds(
                ShieldTransparentRequest::new(self.account_id, threshold),
                &self.seed,
            )
            .await?;
        Ok(prepared
            .iter()
            .flat_map(|tx| tx.transparent_inputs.iter().map(|(outpoint, _)| *outpoint))
            .collect())
    }
}

fn is_insufficient_funds(attempt: &Result<Vec<OutPoint>, StorageError>) -> bool {
    matches!(
        attempt,
        Err(StorageError::ProposalRejected {
            kind: ProposalRejectionKind::InsufficientFunds { .. },
            ..
        })
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shielding_does_not_reselect_an_input_spent_by_a_pending_transaction()
-> Result<(), TestError> {
    let wallet = ShieldingWallet::scanned().await?;
    let utxo = wallet.utxo(0x11);
    let source_outpoint = OutPoint::new(utxo.tx_id, 0);
    wallet
        .storage
        .record_transparent_utxos(None, vec![utxo])
        .await?;

    let transparent_inputs = wallet.shield().await?;
    assert!(
        transparent_inputs.contains(&source_outpoint),
        "the first shielding proposal must select the available input: {transparent_inputs:?}"
    );

    let second_attempt = wallet.shield().await;
    assert!(
        is_insufficient_funds(&second_attempt),
        "the pending wallet transaction must make its input unavailable: {second_attempt:?}"
    );
    Ok(())
}

/// Regression: a coinbase from a block a reorg replaced stayed selectable, so every shielding
/// spent it and the node rejected each one for a missing input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shielding_skips_an_output_its_receiver_no_longer_reports() -> Result<(), TestError> {
    let wallet = ShieldingWallet::scanned().await?;
    let reported = wallet.utxo(0x21);
    let replaced = wallet.utxo(0x22);
    let reported_outpoint = OutPoint::new(reported.tx_id, 0);
    let replaced_outpoint = OutPoint::new(replaced.tx_id, 0);
    wallet
        .record_unspent_set(vec![reported.clone(), replaced])
        .await?;

    let recorded = wallet.record_unspent_set(vec![reported]).await?;
    assert_eq!(recorded.recorded_count, 1);
    assert_eq!(recorded.retired_outpoints, vec![replaced_outpoint]);

    let transparent_inputs = wallet.shield().await?;
    assert_eq!(
        transparent_inputs,
        vec![reported_outpoint],
        "the shielding spends the reported output and leaves the retired one"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_output_the_receiver_reports_again_is_shielded() -> Result<(), TestError> {
    let wallet = ShieldingWallet::scanned().await?;
    let utxo = wallet.utxo(0x31);
    let outpoint = OutPoint::new(utxo.tx_id, 0);
    wallet.record_unspent_set(vec![utxo.clone()]).await?;
    let retired = wallet.record_unspent_set(Vec::new()).await?;
    assert_eq!(retired.retired_outpoints, vec![outpoint]);
    assert!(is_insufficient_funds(&wallet.shield().await));

    let restored = wallet.record_unspent_set(vec![utxo]).await?;
    assert!(restored.retired_outpoints.is_empty());
    assert_eq!(wallet.shield().await?, vec![outpoint]);
    Ok(())
}

/// An output a pending shielding spends keeps its spend record when its receiver omits it.
///
/// The omission may mean the spend was mined. Once the shielding expired unmined, the
/// omission can only mean the output left the chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_output_spent_by_an_unexpired_shielding_keeps_its_spend() -> Result<(), TestError> {
    let wallet = ShieldingWallet::scanned().await?;
    let utxo = wallet.utxo(0x41);
    let outpoint = OutPoint::new(utxo.tx_id, 0);
    wallet.record_unspent_set(vec![utxo]).await?;
    assert_eq!(wallet.shield().await?, vec![outpoint]);

    let while_pending = wallet.record_unspent_set(Vec::new()).await?;
    assert!(
        while_pending.retired_outpoints.is_empty(),
        "an output an unexpired shielding spends is left to its spend record"
    );

    wallet
        .storage
        .update_chain_tip(BlockHeight::from(SCAN_END + 100))
        .await?;
    let after_expiry = wallet.record_unspent_set(Vec::new()).await?;
    assert_eq!(after_expiry.retired_outpoints, vec![outpoint]);
    assert!(
        is_insufficient_funds(&wallet.shield().await),
        "an output whose only spend expired is not selected once its receiver omits it"
    );
    Ok(())
}

/// A transaction that also pays another receiver stays mined on one receiver's omission: the
/// other receiver's output may still be unspent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_output_whose_transaction_pays_another_receiver_is_kept() -> Result<(), TestError> {
    let wallet = ShieldingWallet::scanned().await?;
    let other_script = wallet
        .storage
        .list_transparent_receivers()
        .await?
        .into_iter()
        .map(|receiver| receiver.script_pub_key_bytes)
        .find(|script| *script != wallet.script_pub_key_bytes)
        .ok_or_else(|| TestError::fixture("the wallet has a second transparent receiver"))?;
    let omitted = wallet.utxo(0x61);
    let sibling = TransparentUtxoRow::new(
        omitted.tx_id,
        1,
        omitted.value_zat,
        omitted.mined_height,
        other_script,
    );
    wallet
        .storage
        .record_transparent_utxos(None, vec![omitted, sibling])
        .await?;

    let recorded = wallet.record_unspent_set(Vec::new()).await?;

    assert!(
        recorded.retired_outpoints.is_empty(),
        "one receiver's omission does not un-mine a transaction another receiver holds"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_from_another_network_is_refused() -> Result<(), TestError> {
    let wallet = ShieldingWallet::scanned().await?;
    let refused = wallet
        .storage
        .record_transparent_unspent_set(
            None,
            TransparentUnspentSet::new(
                Network::Mainnet,
                wallet.script_pub_key_bytes.clone(),
                BlockHeight::from(SCAN_END),
                Vec::new(),
            ),
        )
        .await;
    assert!(
        matches!(refused, Err(StorageError::ScanArtifactInvalid { .. })),
        "a set from another network must be refused before it records or retires anything, \
         got {refused:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_output_above_the_observed_height_is_kept() -> Result<(), TestError> {
    let wallet = ShieldingWallet::scanned().await?;
    let utxo = wallet.utxo(0x51);
    let outpoint = OutPoint::new(utxo.tx_id, 0);
    wallet.record_unspent_set(vec![utxo]).await?;

    let recorded = wallet
        .storage
        .record_transparent_unspent_set(
            None,
            TransparentUnspentSet::new(
                Network::Testnet,
                wallet.script_pub_key_bytes.clone(),
                BlockHeight::from(TRANSPARENT_MINED_HEIGHT - 1),
                Vec::new(),
            ),
        )
        .await?;
    assert!(recorded.retired_outpoints.is_empty());
    assert_eq!(wallet.shield().await?, vec![outpoint]);
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
