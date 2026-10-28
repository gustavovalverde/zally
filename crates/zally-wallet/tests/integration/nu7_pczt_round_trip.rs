//! A PCZT created at an NU7 target height round-trips through Zally's PCZT roles and extracts
//! to a version 6 transaction on the NU7 consensus branch with the NU7 default expiry.
//!
//! The wallet runs on a synthetic Regtest chain where every upgrade through NU7 activates at
//! height 1. It shields a recorded transparent UTXO into an Ironwood note, observes that
//! transaction in a compact block, and then creates (storage `create_pczt`), proves
//! (`Prover`), signs (`Signer`), and extracts (storage `extract_and_store_pczt`, the
//! wallet's extraction path) a payment PCZT spending the note.
//!
//! On a chain where NU7 activates after the PCZT is created, the PCZT is bound to the NU6.3
//! branch, and the wallet refuses it once NU7 is the next block, releasing the note it held.

use orchard::tree::MerkleHashOrchard;
use tempfile::TempDir;
use zally_chain::{SubmitterError, UnmineableReason};
use zally_core::{
    BlockHash, BlockHeight, CompactBlockArtifact, CompactChainMetadata, CompactShieldedAction,
    CompactTransaction, CompactTransparentInput, Network, PaymentRecipient, TreeStateArtifact,
    TxId, Zatoshis,
};
use zally_keys::{SealingError, SeedMaterial, SeedSealing as _};
use zally_pczt::{PcztBytes, PcztError, Prover, Signer};
use zally_storage::{
    ProposalPaymentRequest, ScanRequest, ShieldTransparentRequest, Sqlite, SqliteOptions,
    StorageError, TransparentUtxoRow, WalletStorage,
};
use zally_testkit::{InMemorySealing, MockSubmitter};
use zally_wallet::{ProposalPlan, RetryPolicy, Wallet, WalletError};
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

/// A wallet that owns one spendable Ironwood note and has scanned to
/// [`PCZT_CHAIN_TIP_HEIGHT`].
struct IronwoodNoteWallet {
    storage: Sqlite,
    account_id: zally_core::AccountId,
    /// Tree state at [`PCZT_CHAIN_TIP_HEIGHT`], for scanning the blocks above it.
    tip_tree_state: TreeStateArtifact,
    ironwood_tree_size: u32,
}

/// Shields a transparent UTXO into an Ironwood note mined at [`NOTE_MINED_HEIGHT`] and scans
/// to [`PCZT_CHAIN_TIP_HEIGHT`].
async fn wallet_with_ironwood_note(
    network: Network,
    temp: &TempDir,
    seed: &SeedMaterial,
) -> Result<IronwoodNoteWallet, TestError> {
    let (storage, account_id) = open_wallet(network, temp, seed).await?;

    let shielding = shield_into_ironwood(&storage, account_id, seed).await?;
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
    let ironwood_frontier = ironwood_frontier_bytes(&shielding)?;
    let mut anchor_state = empty_tree_state(network, ANCHOR_HEIGHT);
    anchor_state.ironwood_final_state_bytes = ironwood_frontier.clone();
    storage
        .scan_blocks(ScanRequest::new(
            empty_blocks(ANCHOR_HEIGHT + 1..=PCZT_CHAIN_TIP_HEIGHT).collect(),
            BlockHeight::from(ANCHOR_HEIGHT + 1),
            anchor_state,
        ))
        .await?;
    let mut tip_tree_state = empty_tree_state(network, PCZT_CHAIN_TIP_HEIGHT);
    tip_tree_state.ironwood_final_state_bytes = ironwood_frontier;
    Ok(IronwoodNoteWallet {
        storage,
        account_id,
        tip_tree_state,
        ironwood_tree_size,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pczt_at_nu7_round_trips_through_prover_signer_and_extractor() -> Result<(), TestError> {
    let network = regtest_with_nu7_at(1);
    let temp = TempDir::new()?;
    let seed = SeedMaterial::from_raw_bytes(vec![0x5a; 32])
        .map_err(|err| TestError::fixture(err.to_string()))?;
    let IronwoodNoteWallet {
        storage,
        account_id,
        ..
    } = wallet_with_ironwood_note(network, &temp, &seed).await?;

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

/// NU7 activation height for the chain on which a PCZT is created just before NU7.
const LATE_NU7_ACTIVATION_HEIGHT: u32 = PCZT_CHAIN_TIP_HEIGHT + 3;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pczt_created_before_nu7_is_refused_once_nu7_is_next_and_releases_its_note()
-> Result<(), TestError> {
    let network = regtest_with_nu7_at(LATE_NU7_ACTIVATION_HEIGHT);
    let temp = TempDir::new()?;
    let seed = SeedMaterial::from_raw_bytes(vec![0x5a; 32])
        .map_err(|err| TestError::fixture(err.to_string()))?;
    let IronwoodNoteWallet {
        storage,
        account_id,
        tip_tree_state,
        ironwood_tree_size,
    } = wallet_with_ironwood_note(network, &temp, &seed).await?;
    let tip = BlockHeight::from(PCZT_CHAIN_TIP_HEIGHT);
    storage.record_chain_tips(tip, tip).await?;
    let sealing = InMemorySealing::new();
    sealing.seal_seed(&seed).await?;
    let (wallet, _) = Wallet::builder(network, sealing, storage.clone())
        .open()
        .await?;
    let payment = ProposalPlan::conventional(
        account_id,
        PaymentRecipient::UnifiedAddress {
            encoded: storage
                .derive_next_address(account_id)
                .await?
                .encode(&network.to_parameters()),
            network,
        },
        zatoshis(PAYMENT_ZAT)?,
        None,
    );

    // Created, proved, and signed while the next block is still an NU6.3 block.
    let created = wallet.propose_pczt(payment.clone(), None).await?;
    assert_eq!(
        *created.parse()?.global().consensus_branch_id(),
        u32::from(BranchId::Nu6_3)
    );
    let signed = wallet
        .sign_pczt(&wallet.prove_pczt(&created).await?)
        .await?;
    assert!(
        wallet.propose_pczt(payment.clone(), None).await.is_err(),
        "the PCZT holds the wallet's only note"
    );

    // The chain reaches the last NU6.3 block, so the next block is an NU7 block.
    let stale_tip = LATE_NU7_ACTIVATION_HEIGHT - 1;
    storage
        .update_chain_tip(BlockHeight::from(stale_tip))
        .await?;
    storage
        .scan_blocks(ScanRequest::new(
            (PCZT_CHAIN_TIP_HEIGHT + 1..=stale_tip)
                .map(|height| block(height, Vec::new(), ironwood_tree_size))
                .collect(),
            BlockHeight::from(PCZT_CHAIN_TIP_HEIGHT + 1),
            tip_tree_state,
        ))
        .await?;
    let stale_tip = BlockHeight::from(stale_tip);
    storage.record_chain_tips(stale_tip, stale_tip).await?;

    let crossed = Some(UnmineableReason::ConsensusBranchMismatch {
        signed_branch_id: BranchId::Nu6_3,
        next_branch_id: BranchId::Nu7,
    });
    assert_eq!(
        unmineable_reason(&wallet.extract_pczt(&signed).await),
        crossed,
        "extraction refuses the stale PCZT before storing its transaction"
    );
    assert_eq!(
        unmineable_reason(&wallet.prove_pczt(&created).await),
        crossed
    );
    assert_eq!(
        unmineable_reason(&wallet.sign_pczt(&created).await),
        crossed
    );

    let recreated = wallet.propose_pczt(payment, None).await?;
    assert_eq!(
        *recreated.parse()?.global().consensus_branch_id(),
        u32::from(BranchId::Nu7),
        "the released note funds a PCZT for the NU7 branch"
    );
    Ok(())
}

fn unmineable_reason<T>(outcome: &Result<T, WalletError>) -> Option<UnmineableReason> {
    match outcome {
        Err(WalletError::TransactionUnmineable { reason }) => Some(*reason),
        _ => None,
    }
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
    #[error("sealing error: {0}")]
    Sealing(#[from] SealingError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("fixture error: {0}")]
    Fixture(String),
}

impl TestError {
    fn fixture(reason: impl Into<String>) -> Self {
        Self::Fixture(reason.into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pczt_submission_retried_after_a_lost_acknowledgement_submits_the_same_transaction()
-> Result<(), TestError> {
    let network = regtest_with_nu7_at(1);
    let temp = TempDir::new()?;
    let seed = SeedMaterial::from_raw_bytes(vec![0x5a; 32])
        .map_err(|err| TestError::fixture(err.to_string()))?;
    let IronwoodNoteWallet {
        storage,
        account_id,
        ..
    } = wallet_with_ironwood_note(network, &temp, &seed).await?;
    let tip = BlockHeight::from(PCZT_CHAIN_TIP_HEIGHT);
    storage.record_chain_tips(tip, tip).await?;
    let sealing = InMemorySealing::new();
    sealing.seal_seed(&seed).await?;
    let (wallet, _) = Wallet::builder(network, sealing, storage.clone())
        .open()
        .await?;
    wallet.set_retry_policy(RetryPolicy::none());
    let payment = ProposalPlan::conventional(
        account_id,
        PaymentRecipient::UnifiedAddress {
            encoded: storage
                .derive_next_address(account_id)
                .await?
                .encode(&network.to_parameters()),
            network,
        },
        zatoshis(PAYMENT_ZAT)?,
        None,
    );
    let created = wallet.propose_pczt(payment, None).await?;
    let signed = wallet
        .sign_pczt(&wallet.prove_pczt(&created).await?)
        .await?;
    let submitter = MockSubmitter::accepting(network).with_tx_id_derivation(nu7_transaction_id);
    let submissions = submitter.handle();
    submissions.fail_submit_next(1, || SubmitterError::Unavailable {
        reason: "acknowledgement lost".to_owned(),
    });

    let first = wallet.extract_and_submit_pczt(&signed, &submitter).await;
    assert!(
        matches!(&first, Err(error) if error.is_retryable()),
        "the lost acknowledgement surfaces as a retryable error: {first:?}"
    );
    let retried = wallet.extract_and_submit_pczt(&signed, &submitter).await?;
    let received = submissions.submitted_bytes();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received.first().map(|raw| nu7_transaction_id(raw)),
        Some(retried.tx_id()),
        "the retry submits the transaction the PCZT already extracted"
    );
    assert_eq!(wallet.extract_pczt(&signed).await?, retried.tx_id());
    Ok(())
}

fn nu7_transaction_id(raw_tx: &[u8]) -> TxId {
    Transaction::read(raw_tx, BranchId::Nu7).map_or(TxId::from_bytes([0; 32]), |transaction| {
        TxId::from_bytes(*transaction.txid().as_ref())
    })
}
