//! Shared fixture for the reorg and ledger tests.
//!
//! A [`FundedChain`] is a Regtest wallet on the branch-aware mock chain. The wallet shields a
//! transparent UTXO into Sapling through `Wallet::shield_transparent_funds`, so the payment
//! ledger records an idempotency key for it, and the mock mines that transaction a few blocks
//! below the tip. A reorg can then remove a transaction the wallet sent and a note it received
//! together, as a reverted withdrawal and a reverted deposit.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use zally_chain::{ChainSource, ShieldedPool};
use zally_core::{
    AccountId, BlockHeight, CompactChainMetadata, CompactSaplingOutput, CompactTransaction,
    CompactTransparentInput, HoldId, IdempotencyKey, Network, TxId, Zatoshis,
};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{HoldRecord, Sqlite, SqliteOptions, TransparentUtxoRow, WalletStorage};
use zally_testkit::{
    MockChainSource, MockChainSourceHandle, MockCommitmentTrees, MockSubmitter, TempWalletPath,
};
use zally_wallet::{
    ShieldTransparentPlan, SyncDriver, SyncDriverOptions, SyncHandle, SyncRecoveryPolicy,
    SyncSnapshot, Wallet, WalletEvent, WalletEventStream,
};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BranchId;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_transparent::address::Script;

/// Funding transaction for the transparent UTXO the wallet shields.
pub(crate) const TRANSPARENT_FUNDING_TX_ID: [u8; 32] = [0x77; 32];
const TRANSPARENT_UTXO_ZAT: u64 = 1_000_000;
pub(crate) const SHIELDING_KEY: &str = "withdrawal-0001";
const HOLD_REQUEST: &str = "hold-request-0001";

/// Every upgrade through NU7 activates at height 1.
pub(crate) fn regtest_with_nu7_from_height_one() -> Network {
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

/// A wallet with its storage handle on a mock chain, before any funding.
pub(crate) struct ChainWallet {
    pub temp: TempWalletPath,
    pub wallet: Wallet,
    pub storage: Sqlite,
    pub account_id: AccountId,
    pub chain: Arc<MockChainSource>,
    pub chain_handle: MockChainSourceHandle,
    pub network: Network,
}

/// Creates a wallet at birthday 1 on a mock chain whose tip is `tip_height`.
pub(crate) async fn chain_wallet(tip_height: u32) -> Result<ChainWallet, FixtureError> {
    let network = regtest_with_nu7_from_height_one();
    let temp = TempWalletPath::create()?;
    let (spend_bytes, output_bytes) = wagyu_zcash_parameters::load_sapling_parameters();
    let spend_path = temp.dir().join(zcash_proofs::SAPLING_SPEND_NAME);
    let output_path = temp.dir().join(zcash_proofs::SAPLING_OUTPUT_NAME);
    std::fs::write(&spend_path, spend_bytes)?;
    std::fs::write(&output_path, output_bytes)?;
    let storage = Sqlite::new(
        SqliteOptions::for_network(network, temp.db_path())
            .with_sapling_proving_parameters(spend_path, output_path),
    );
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let chain = Arc::new(MockChainSource::new(network));
    let chain_handle = chain.handle();
    chain_handle.advance_tip(BlockHeight::from(tip_height));
    let (wallet, account_id, _mnemonic) = Wallet::builder(network, sealing, storage.clone())
        .create(chain.as_ref(), BlockHeight::from(1))
        .await?;
    Ok(ChainWallet {
        temp,
        wallet,
        storage,
        account_id,
        chain,
        chain_handle,
        network,
    })
}

/// A [`ChainWallet`] whose shielding transaction the mock mined at `funding_height`.
pub(crate) struct FundedChain {
    pub base: ChainWallet,
    pub submitter: Arc<MockSubmitter>,
    pub shielding: Transaction,
    pub shielding_tx_id: TxId,
    pub shielding_key: IdempotencyKey,
    pub funding_height: u32,
    pub tip_height: u32,
}

impl FundedChain {
    /// Compact form of the shielding transaction, as a chain source serves it.
    pub(crate) fn compact_shielding(&self) -> Result<CompactTransaction, FixtureError> {
        compact_sapling_shielding(&self.shielding)
    }

    /// Mines the shielding transaction at `height` on the branch that currently holds it.
    pub(crate) fn mine_shielding_at(&self, height: u32) -> Result<(), FixtureError> {
        self.base.chain_handle.mine_transactions(
            BlockHeight::from(height),
            vec![self.compact_shielding()?],
            sapling_trees_after(&self.shielding)?,
        );
        Ok(())
    }
}

/// Funds a wallet: syncs `pre_funding_tip` empty blocks, shields a transparent UTXO through
/// the wallet, and mines the shielding transaction at `pre_funding_tip + 1` with the tip two
/// blocks above it.
pub(crate) async fn funded_chain(pre_funding_tip: u32) -> Result<FundedChain, FixtureError> {
    let base = chain_wallet(pre_funding_tip.saturating_sub(100)).await?;
    // A scan chunk ends at a checkpoint; the one at `pre_funding_tip - 100` anchors the
    // shielding proposal's 100-confirmation policy.
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    base.chain_handle
        .advance_tip(BlockHeight::from(pre_funding_tip));
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    record_transparent_utxo(&base, TRANSPARENT_FUNDING_TX_ID).await?;

    let submitter = Arc::new(
        MockSubmitter::accepting(base.network).with_tx_id_derivation(parsed_transaction_id),
    );
    let shielding_key = IdempotencyKey::try_from(SHIELDING_KEY.to_owned())
        .map_err(|_| FixtureError::Fixture("idempotency key"))?;
    let outcome = base
        .wallet
        .shield_transparent_funds(
            ShieldTransparentPlan::new(
                base.account_id,
                shielding_key.clone(),
                zatoshis(1_000)?,
                submitter.as_ref(),
            )
            .with_destination_pool(ShieldedPool::Sapling),
        )
        .await?;
    let raw = submitter
        .handle()
        .submitted_bytes()
        .pop()
        .ok_or(FixtureError::Fixture("the shielding was not submitted"))?;
    let shielding = Transaction::read(raw.as_slice(), BranchId::Nu7)?;
    let shielding_tx_id = TxId::from_bytes(*shielding.txid().as_ref());
    if outcome.tx_id() != shielding_tx_id {
        return Err(FixtureError::Fixture(
            "submitted bytes differ from the outcome",
        ));
    }

    let funding_height = pre_funding_tip + 1;
    let tip_height = funding_height + 2;
    let funded = FundedChain {
        base,
        submitter,
        shielding,
        shielding_tx_id,
        shielding_key,
        funding_height,
        tip_height,
    };
    funded.mine_shielding_at(funding_height)?;
    funded
        .base
        .chain_handle
        .advance_tip(BlockHeight::from(tip_height));
    Ok(funded)
}

/// Records a transparent UTXO the wallet owns, created by `funding_tx_id` at height 1.
pub(crate) async fn record_transparent_utxo(
    base: &ChainWallet,
    funding_tx_id: [u8; 32],
) -> Result<(), FixtureError> {
    let address = base
        .storage
        .derive_next_address_with_transparent(base.account_id)
        .await?;
    let transparent = address.transparent().copied().ok_or(FixtureError::Fixture(
        "derived address has no transparent receiver",
    ))?;
    base.storage
        .record_transparent_utxos(
            None,
            vec![TransparentUtxoRow::new(
                TxId::from_bytes(funding_tx_id),
                0,
                zatoshis(TRANSPARENT_UTXO_ZAT)?,
                BlockHeight::from(1),
                Script::from(transparent.script()).0.0,
            )],
        )
        .await?;
    Ok(())
}

/// Records a hold in the wallet's ledger, as a dispense reservation would.
pub(crate) async fn record_hold(
    storage: &Sqlite,
    account_id: AccountId,
) -> Result<HoldId, FixtureError> {
    let hold_id = HoldId::new();
    storage
        .create_hold(HoldRecord {
            hold_id,
            request_id: IdempotencyKey::try_from(HOLD_REQUEST.to_owned())
                .map_err(|_| FixtureError::Fixture("hold request id"))?,
            idempotency_key: IdempotencyKey::try_from(SHIELDING_KEY.to_owned())
                .map_err(|_| FixtureError::Fixture("hold idempotency key"))?,
            account_id,
            amount_zat: zatoshis(10_000)?,
            spendable_for_check_zat: zatoshis(100_000_000)?,
            locked_notes: Vec::new(),
            reserved_at_ms: 1_700_000_000_000,
        })
        .await?;
    Ok(hold_id)
}

/// Drives `Wallet::sync` and `Wallet::scan_queued_range` until the wallet reaches the tip.
pub(crate) async fn sync_to_tip(
    wallet: &Wallet,
    chain: &dyn ChainSource,
) -> Result<(), FixtureError> {
    for _ in 0..64 {
        if wallet.sync(chain).await?.block_count == 0 {
            return Ok(());
        }
        while wallet.scan_queued_range(chain).await?.block_count > 0 {}
    }
    Err(FixtureError::Fixture("the wallet never reached the tip"))
}

/// Driver options with fast polling and recovery, and the stall check off.
pub(crate) fn fast_driver_options() -> SyncDriverOptions {
    SyncDriverOptions::default()
        .with_poll_interval_ms(20)
        .with_visible_tip_stall_limit_ms(None)
        .with_recovery_policy(
            SyncRecoveryPolicy::default()
                .with_fault_backoff_initial_ms(10)
                .with_fault_backoff_cap_ms(20)
                .with_park_reprobe_ms(None),
        )
}

/// Starts a driver for `wallet` on `chain`, resubmitting through `submitter` when given.
pub(crate) fn start_driver(
    wallet: &Wallet,
    chain: &Arc<MockChainSource>,
    submitter: Option<&Arc<MockSubmitter>>,
    options: SyncDriverOptions,
) -> Result<SyncHandle, FixtureError> {
    let chain_source: Arc<dyn ChainSource> = Arc::clone(chain) as Arc<dyn ChainSource>;
    let mut driver = SyncDriver::new(wallet.clone(), chain_source, options)?;
    if let Some(submitter) = submitter {
        driver = driver.with_submitter(Arc::clone(submitter) as Arc<dyn zally_chain::Submitter>)?;
    }
    Ok(driver.sync_continuously())
}

/// Longest any reorg test waits for the driver.
const DRIVER_TIMEOUT: Duration = Duration::from_mins(1);

/// Waits until a driver snapshot satisfies `predicate`, recording every snapshot it sees.
pub(crate) async fn wait_for_driver(
    handle: &SyncHandle,
    seen: &mut Vec<SyncSnapshot>,
    mut predicate: impl FnMut(&SyncSnapshot) -> bool,
) -> Result<SyncSnapshot, FixtureError> {
    let mut snapshots = handle.observe_status();
    tokio::time::timeout(DRIVER_TIMEOUT, async {
        while let Some(snapshot) = snapshots.next().await {
            seen.push(snapshot.clone());
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
        }
        Err(FixtureError::Fixture("the snapshot stream closed"))
    })
    .await
    .map_err(|_elapsed| FixtureError::Fixture("timed out waiting for the driver"))?
}

/// Collects wallet events in the background for later assertions.
pub(crate) struct EventLog(Arc<Mutex<Vec<WalletEvent>>>);

impl EventLog {
    pub(crate) fn record(mut events: WalletEventStream) -> Self {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        drop(tokio::spawn(async move {
            while let Some(event) = events.next().await {
                sink.lock().push(event);
            }
        }));
        Self(log)
    }

    pub(crate) fn snapshot(&self) -> Vec<WalletEvent> {
        self.0.lock().clone()
    }

    /// Waits until a recorded event satisfies `predicate`.
    pub(crate) async fn wait_for(
        &self,
        mut predicate: impl FnMut(&WalletEvent) -> bool,
    ) -> Result<(), FixtureError> {
        tokio::time::timeout(DRIVER_TIMEOUT, async {
            loop {
                if self.0.lock().iter().any(&mut predicate) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_elapsed| FixtureError::Fixture("timed out waiting for a wallet event"))
    }
}

fn compact_sapling_shielding(
    transaction: &Transaction,
) -> Result<CompactTransaction, FixtureError> {
    let bundle = transaction.sapling_bundle().ok_or(FixtureError::Fixture(
        "Sapling shielding must carry a Sapling bundle",
    ))?;
    let sapling_outputs = bundle
        .shielded_outputs()
        .iter()
        .map(|output| {
            let ciphertext: [u8; 52] = output.enc_ciphertext()[..52]
                .try_into()
                .map_err(|_| FixtureError::Fixture("Sapling ciphertext is too short"))?;
            Ok(CompactSaplingOutput {
                commitment_bytes: output.cmu().to_bytes(),
                ephemeral_key_bytes: output.ephemeral_key().0,
                ciphertext_bytes: ciphertext,
            })
        })
        .collect::<Result<Vec<_>, FixtureError>>()?;
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

/// Commitment trees after a block holding only `transaction`, on a chain whose trees were
/// empty before it.
fn sapling_trees_after(transaction: &Transaction) -> Result<MockCommitmentTrees, FixtureError> {
    let bundle = transaction.sapling_bundle().ok_or(FixtureError::Fixture(
        "Sapling shielding must carry a Sapling bundle",
    ))?;
    let mut tree = incrementalmerkletree::frontier::CommitmentTree::<sapling::Node, 32>::empty();
    for output in bundle.shielded_outputs() {
        tree.append(sapling::Node::from_cmu(output.cmu()))
            .map_err(|()| FixtureError::Fixture("synthetic Sapling tree is full"))?;
    }
    let mut sapling_frontier_bytes = Vec::new();
    zcash_primitives::merkle_tree::write_commitment_tree(&tree, &mut sapling_frontier_bytes)?;
    let sapling_commitment_tree_size = u32::try_from(bundle.shielded_outputs().len())
        .map_err(|_| FixtureError::Fixture("output count overflows u32"))?;
    Ok(MockCommitmentTrees {
        sapling_frontier_bytes,
        orchard_frontier_bytes: vec![0, 0, 0],
        ironwood_frontier_bytes: vec![0, 0, 0],
        chain_metadata: CompactChainMetadata {
            sapling_commitment_tree_size,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        },
    })
}

/// Transaction id of a serialized NU7 transaction, or all zeros when it does not parse.
fn parsed_transaction_id(raw_tx: &[u8]) -> TxId {
    Transaction::read(raw_tx, BranchId::Nu7).map_or(TxId::from_bytes([0; 32]), |transaction| {
        TxId::from_bytes(*transaction.txid().as_ref())
    })
}

fn zatoshis(zat: u64) -> Result<Zatoshis, FixtureError> {
    Zatoshis::try_from(zat).map_err(|_| FixtureError::Fixture("zatoshi value out of range"))
}

/// Error returned by the reorg fixture and the tests built on it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FixtureError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] zally_wallet::WalletError),
    #[error("storage error: {0}")]
    Storage(#[from] zally_storage::StorageError),
    #[error("fixture error: {0}")]
    Fixture(&'static str),
}
