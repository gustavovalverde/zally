//! A broadcast built under NU6.3 and still pending when NU7 activates is reported as unmineable
//! exactly once, its recorded inputs are released, and its idempotency key then creates one
//! replacement.
//!
//! The wallet runs on a Regtest chain where every upgrade through NU6.3 activates at height 1
//! and NU7 at height 120. At tip 110 it shields a transparent UTXO through
//! `Wallet::shield_transparent_funds`, which builds a version 6 transaction on the NU6.3 branch
//! that expires at height 151 and records its transparent input as pending. The chain never
//! mines it. Each `Wallet::sync` judges the broadcast at the lower of the scanned height and the
//! settled tip; once the block after that height is an NU7 block, the broadcast can never be
//! mined. The wallet flags pending inputs as stale after one millisecond, which must never
//! retire them.

use std::time::Duration;

use parking_lot::Mutex;
use tempfile::TempDir;
use zally_chain::{
    SubmitOutcome, Submitter, SubmitterError, parse_transaction_header, parse_transaction_id,
};
use zally_core::{AccountId, BlockHeight, BranchId, IdempotencyKey, Network, TxId, Zatoshis};
use zally_storage::{Sqlite, SqliteOptions, StorageError, TransparentUtxoRow, WalletStorage as _};
use zally_testkit::{InMemorySealing, MockChainSource, MockChainSourceHandle};
use zally_wallet::{
    SendOutcome, ShieldTransparentPlan, Wallet, WalletError, WalletEvent, WalletEventStream,
    WalletOptions,
};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_transparent::address::Script;

const NU7_ACTIVATION_HEIGHT: u32 = 120;
const BUILD_TIP_HEIGHT: u32 = 110;
const ANCHOR_WINDOW_START_HEIGHT: u32 = 100;
const TRANSPARENT_FUNDING_TX_ID: [u8; 32] = [0x77; 32];
const REPLACEMENT_FUNDING_TX_ID: [u8; 32] = [0x78; 32];
/// Blocks the settled tip trails the visible tip by when Zinder's reorg window is 1000.
const SETTLED_LAG_BLOCKS: u32 = 1_000;
const SHIELD_KEY: &str = "shield-before-nu7";
const TRANSPARENT_UTXO_ZAT: u64 = 1_000_000;
const SHIELDING_THRESHOLD_ZAT: u64 = 1_000;

fn regtest_with_nu7_at_one_hundred_twenty() -> Network {
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
            NU7_ACTIVATION_HEIGHT,
        )),
    })
}

/// Moves the mock chain to `visible`/`settled` and syncs until the wallet reports no blocks.
async fn sync_to(
    wallet: &Wallet,
    chain: &MockChainSource,
    chain_handle: &MockChainSourceHandle,
    visible: u32,
    settled: u32,
) -> Result<(), TestError> {
    if !chain_handle.set_chain_tips(BlockHeight::from(visible), BlockHeight::from(settled)) {
        return Err(TestError::Fixture("settled tip above visible tip"));
    }
    while wallet.sync(chain).await?.block_count > 0 {}
    Ok(())
}

/// Drains the events already published and returns the unmineable-broadcast reports.
async fn unmineable_reports(events: &mut WalletEventStream) -> Vec<(TxId, String)> {
    let mut reports = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(200), events.next()).await
    {
        if let WalletEvent::PendingBroadcastUnmineable { tx_id, reason } = event {
            reports.push((tx_id, reason));
        }
    }
    reports
}

/// A wallet on the mock chain, synced to [`BUILD_TIP_HEIGHT`], with one pending shielding
/// broadcast that the chain never mines.
struct PendingShieldingWallet {
    wallet: Wallet,
    account_id: AccountId,
    storage: Sqlite,
    chain: MockChainSource,
    chain_handle: MockChainSourceHandle,
    submitter: AcceptingSubmitter,
    pending_tx_id: TxId,
    /// Holds the wallet database and proving parameters for the wallet's lifetime.
    temp: TempDir,
}

async fn wallet_with_pending_shielding() -> Result<PendingShieldingWallet, TestError> {
    let network = regtest_with_nu7_at_one_hundred_twenty();
    let temp = TempDir::new()?;
    let (spend_bytes, output_bytes) = wagyu_zcash_parameters::load_sapling_parameters();
    let spend_path = temp.path().join(zcash_proofs::SAPLING_SPEND_NAME);
    let output_path = temp.path().join(zcash_proofs::SAPLING_OUTPUT_NAME);
    std::fs::write(&spend_path, spend_bytes)?;
    std::fs::write(&output_path, output_bytes)?;
    let storage = Sqlite::new(
        SqliteOptions::for_network(network, temp.path().join("wallet.db"))
            .with_sapling_proving_parameters(spend_path, output_path),
    );
    let chain = MockChainSource::new(network);
    let chain_handle = chain.handle();
    chain_handle.serve_compact_blocks();
    let (wallet, account_id, _mnemonic) =
        Wallet::builder(network, InMemorySealing::new(), storage.clone())
            .with_options(WalletOptions::default().with_pending_broadcast_window_ms(1))
            .create(&chain, BlockHeight::from(1))
            .await?;
    sync_block_by_block(
        &wallet,
        &chain,
        &chain_handle,
        ANCHOR_WINDOW_START_HEIGHT,
        BUILD_TIP_HEIGHT,
    )
    .await?;
    record_transparent_utxo(&wallet, &storage, account_id, TRANSPARENT_FUNDING_TX_ID).await?;
    let submitter = AcceptingSubmitter::new(network);
    let pending_tx_id = shield(&wallet, account_id, &submitter).await?.tx_id();
    Ok(PendingShieldingWallet {
        wallet,
        account_id,
        storage,
        chain,
        chain_handle,
        submitter,
        pending_tx_id,
        temp,
    })
}

/// Syncs to `from`, then one block at a time to `to`. A proposal anchors at a checkpoint, and
/// each scanned chunk checkpoints only its last block.
async fn sync_block_by_block(
    wallet: &Wallet,
    chain: &MockChainSource,
    chain_handle: &MockChainSourceHandle,
    from: u32,
    to: u32,
) -> Result<(), TestError> {
    sync_to(wallet, chain, chain_handle, from, from).await?;
    for height in from + 1..=to {
        sync_to(wallet, chain, chain_handle, height, height).await?;
    }
    Ok(())
}

async fn record_transparent_utxo(
    wallet: &Wallet,
    storage: &Sqlite,
    account_id: AccountId,
    funding_tx_id: [u8; 32],
) -> Result<(), TestError> {
    let address = wallet
        .derive_next_address_with_transparent(account_id)
        .await?;
    let transparent = address.transparent().copied().ok_or(TestError::Fixture(
        "derived address has no transparent receiver",
    ))?;
    storage
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

async fn shield(
    wallet: &Wallet,
    account_id: AccountId,
    submitter: &AcceptingSubmitter,
) -> Result<SendOutcome, TestError> {
    Ok(wallet
        .shield_transparent_funds(ShieldTransparentPlan::new(
            account_id,
            IdempotencyKey::try_from(SHIELD_KEY)
                .map_err(|_| TestError::Fixture("invalid idempotency key"))?,
            zatoshis(SHIELDING_THRESHOLD_ZAT)?,
            submitter,
        ))
        .await?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_signed_before_nu7_is_reported_once_across_a_long_settled_lag()
-> Result<(), TestError> {
    let PendingShieldingWallet {
        wallet,
        account_id,
        chain,
        chain_handle,
        pending_tx_id,
        temp: _temp,
        ..
    } = wallet_with_pending_shielding().await?;
    assert_eq!(
        pending_inputs_of(&wallet, account_id).await?,
        vec![pending_tx_id],
        "the shielding broadcast records its transparent input"
    );
    let mut events = wallet.observe();
    tokio::time::sleep(Duration::from_millis(10)).await;

    // The chain runs far past NU7 while the settled tip stays on the last block whose
    // successor is still an NU6.3 block, as it does behind Zinder's reorg window.
    let lagging_settled = NU7_ACTIVATION_HEIGHT - 2;
    sync_to(
        &wallet,
        &chain,
        &chain_handle,
        lagging_settled + SETTLED_LAG_BLOCKS,
        lagging_settled,
    )
    .await?;
    assert!(unmineable_reports(&mut events).await.is_empty());
    let snapshot = wallet.get_pending_transparent_inputs(account_id).await?;
    assert_eq!(snapshot.inputs.len(), 1, "an old, unjudged input is kept");
    assert!(snapshot.inputs[0].is_stale, "and flagged as stale");

    // The settled tip reaches the last NU6.3 block, so the next block is an NU7 block.
    let settled = NU7_ACTIVATION_HEIGHT - 1;
    sync_to(
        &wallet,
        &chain,
        &chain_handle,
        settled + SETTLED_LAG_BLOCKS,
        settled,
    )
    .await?;
    let reports = unmineable_reports(&mut events).await;
    assert_eq!(reports.len(), 1, "one report for the stale broadcast");
    let (reported_tx_id, reason) = &reports[0];
    assert_eq!(*reported_tx_id, pending_tx_id);
    assert!(
        reason.contains("Nu6_3") && reason.contains("Nu7"),
        "the reason names both consensus branches: {reason}"
    );
    assert!(
        pending_inputs_of(&wallet, account_id).await?.is_empty(),
        "the stale broadcast's recorded inputs are released"
    );

    let caught_up = settled + SETTLED_LAG_BLOCKS + 10;
    sync_to(&wallet, &chain, &chain_handle, caught_up, caught_up).await?;
    assert!(
        unmineable_reports(&mut events).await.is_empty(),
        "a later sync does not report it again"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmineable_broadcast_frees_its_idempotency_key_for_one_replacement()
-> Result<(), TestError> {
    let PendingShieldingWallet {
        wallet,
        account_id,
        storage,
        chain,
        chain_handle,
        submitter,
        pending_tx_id,
        temp: _temp,
    } = wallet_with_pending_shielding().await?;

    // While the broadcast can still be mined, its key answers with it, submitted again.
    assert_eq!(
        shield(&wallet, account_id, &submitter).await?.tx_id(),
        pending_tx_id
    );
    assert_eq!(submitter.distinct_transaction_count(), 1);

    let mut events = wallet.observe();
    let settled = NU7_ACTIVATION_HEIGHT - 1;
    sync_to(&wallet, &chain, &chain_handle, settled + 6, settled).await?;
    assert_eq!(unmineable_reports(&mut events).await.len(), 1);

    // The unmineable broadcast's own input stays excluded by librustzcash until its expiry,
    // so the replacement shields a second UTXO.
    record_transparent_utxo(&wallet, &storage, account_id, REPLACEMENT_FUNDING_TX_ID).await?;
    sync_block_by_block(&wallet, &chain, &chain_handle, settled + 7, settled + 11).await?;

    let (first_retry, second_retry) = tokio::join!(
        shield(&wallet, account_id, &submitter),
        shield(&wallet, account_id, &submitter),
    );
    let replacement_tx_id = first_retry?.tx_id();
    assert_ne!(replacement_tx_id, pending_tx_id);
    assert_eq!(
        second_retry?.tx_id(),
        replacement_tx_id,
        "a concurrent retry with the same key gets the same replacement"
    );
    assert_eq!(
        submitter.distinct_transaction_count(),
        2,
        "the concurrent retries broadcast one replacement"
    );
    let replacement_bytes = submitter
        .last_submission()
        .ok_or(TestError::Fixture("no replacement was submitted"))?;
    assert_eq!(
        parse_transaction_header(&replacement_bytes)
            .map_err(|_| TestError::Fixture("replacement does not parse"))?
            .consensus_branch_id(),
        Some(BranchId::Nu7)
    );

    assert_eq!(
        shield(&wallet, account_id, &submitter).await?.tx_id(),
        replacement_tx_id,
        "the key answers with the pending replacement"
    );
    assert_eq!(submitter.distinct_transaction_count(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_syncs_report_an_unmineable_broadcast_once() -> Result<(), TestError> {
    let PendingShieldingWallet {
        wallet,
        chain,
        chain_handle,
        pending_tx_id,
        temp: _temp,
        ..
    } = wallet_with_pending_shielding().await?;
    let mut events = wallet.observe();
    let settled = NU7_ACTIVATION_HEIGHT - 1;
    if !chain_handle.set_chain_tips(BlockHeight::from(settled + 6), BlockHeight::from(settled)) {
        return Err(TestError::Fixture("settled tip above visible tip"));
    }
    let (first, second, third) = tokio::join!(
        wallet.sync(&chain),
        wallet.sync(&chain),
        wallet.sync(&chain)
    );
    first?;
    second?;
    third?;
    sync_to(&wallet, &chain, &chain_handle, settled + 6, settled).await?;
    let reports = unmineable_reports(&mut events).await;
    assert_eq!(
        reports.iter().map(|(tx_id, _)| *tx_id).collect::<Vec<_>>(),
        vec![pending_tx_id],
        "concurrent syncs must report the unmineable broadcast exactly once"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_birthday_rebuild_keeps_judging_an_in_flight_broadcast() -> Result<(), TestError> {
    let PendingShieldingWallet {
        wallet,
        storage,
        chain,
        chain_handle,
        pending_tx_id,
        temp: _temp,
        ..
    } = wallet_with_pending_shielding().await?;
    wallet.reset_to_birthday(&chain).await?;
    let shield_key = IdempotencyKey::try_from(SHIELD_KEY)
        .map_err(|_| TestError::Fixture("invalid idempotency key"))?;
    assert_eq!(
        storage.find_idempotent_submission(&shield_key).await?,
        Some(pending_tx_id),
        "the rebuild keeps the in-flight broadcast's key"
    );

    let mut events = wallet.observe();
    let settled = NU7_ACTIVATION_HEIGHT - 1;
    sync_to(&wallet, &chain, &chain_handle, settled + 6, settled).await?;
    let reports = unmineable_reports(&mut events).await;
    assert_eq!(
        reports.iter().map(|(tx_id, _)| *tx_id).collect::<Vec<_>>(),
        vec![pending_tx_id],
        "the rebuilt wallet must still judge the in-flight broadcast"
    );
    assert_eq!(
        storage.find_idempotent_submission(&shield_key).await?,
        None,
        "the verdict frees the key after the rebuild"
    );
    Ok(())
}

/// Accepts every transaction and echoes its canonical transaction ID, as a node does.
struct AcceptingSubmitter {
    network: Network,
    submitted: Mutex<Vec<Vec<u8>>>,
}

impl AcceptingSubmitter {
    fn new(network: Network) -> Self {
        Self {
            network,
            submitted: Mutex::new(Vec::new()),
        }
    }

    /// Number of distinct transactions submitted; a retry submits the same bytes again.
    fn distinct_transaction_count(&self) -> usize {
        let submitted = self.submitted.lock().clone();
        let mut distinct: Vec<Vec<u8>> = Vec::new();
        for raw_tx in submitted {
            if !distinct.contains(&raw_tx) {
                distinct.push(raw_tx);
            }
        }
        distinct.len()
    }

    fn last_submission(&self) -> Option<Vec<u8>> {
        self.submitted.lock().last().cloned()
    }
}

#[async_trait::async_trait]
impl Submitter for AcceptingSubmitter {
    fn network(&self) -> Network {
        self.network
    }

    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        let tx_id =
            parse_transaction_id(raw_tx).map_err(|err| SubmitterError::RequestRejected {
                reason: err.to_string(),
            })?;
        self.submitted.lock().push(raw_tx.to_vec());
        Ok(SubmitOutcome::Accepted { tx_id })
    }
}

async fn pending_inputs_of(
    wallet: &Wallet,
    account_id: AccountId,
) -> Result<Vec<TxId>, WalletError> {
    Ok(wallet
        .get_pending_transparent_inputs(account_id)
        .await?
        .inputs
        .into_iter()
        .map(|input| input.broadcast_tx_id)
        .collect())
}

fn zatoshis(zat: u64) -> Result<Zatoshis, TestError> {
    Zatoshis::try_from(zat).map_err(|_| TestError::Fixture("zatoshis out of range"))
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("fixture error: {0}")]
    Fixture(&'static str),
}
