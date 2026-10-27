//! A broadcast built under NU6.3 and still pending when NU7 activates is reported as unmineable,
//! and its recorded inputs are released.
//!
//! The wallet runs on a Regtest chain where every upgrade through NU6.3 activates at height 1
//! and NU7 at height 120. At tip 110 it shields a transparent UTXO through
//! `Wallet::shield_transparent_funds`, which builds a version 6 transaction on the NU6.3 branch
//! that expires at height 151 and records its transparent input as pending. The chain never
//! mines it. Each `Wallet::sync` judges the broadcast at the lower of the scanned height and the
//! settled tip; once the block after that height is an NU7 block, the broadcast can never be
//! mined.

use std::time::Duration;

use tempfile::TempDir;
use zally_chain::{SubmitOutcome, Submitter, SubmitterError, parse_transaction_id};
use zally_core::{BlockHeight, IdempotencyKey, Network, TxId, Zatoshis};
use zally_storage::{Sqlite, SqliteOptions, StorageError, TransparentUtxoRow, WalletStorage as _};
use zally_testkit::{InMemorySealing, MockChainSource, MockChainSourceHandle};
use zally_wallet::{ShieldTransparentPlan, Wallet, WalletError, WalletEvent, WalletEventStream};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_transparent::address::Script;

const NU7_ACTIVATION_HEIGHT: u32 = 120;
const BUILD_TIP_HEIGHT: u32 = 110;
const ANCHOR_WINDOW_START_HEIGHT: u32 = 100;
const TRANSPARENT_FUNDING_TX_ID: [u8; 32] = [0x77; 32];
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
    account_id: zally_core::AccountId,
    chain: MockChainSource,
    chain_handle: MockChainSourceHandle,
    pending_tx_id: TxId,
    /// Holds the wallet database and proving parameters for the wallet's lifetime.
    temp: TempDir,
}

async fn wallet_with_pending_shielding(
    network: Network,
) -> Result<PendingShieldingWallet, TestError> {
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
            .create(&chain, BlockHeight::from(1))
            .await?;
    // A proposal anchors at a checkpoint, and each scanned chunk checkpoints only its last
    // block, so the blocks below the tip are scanned one at a time.
    sync_to(
        &wallet,
        &chain,
        &chain_handle,
        ANCHOR_WINDOW_START_HEIGHT,
        ANCHOR_WINDOW_START_HEIGHT,
    )
    .await?;
    for height in ANCHOR_WINDOW_START_HEIGHT + 1..=BUILD_TIP_HEIGHT {
        sync_to(&wallet, &chain, &chain_handle, height, height).await?;
    }

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
                TxId::from_bytes(TRANSPARENT_FUNDING_TX_ID),
                0,
                zatoshis(TRANSPARENT_UTXO_ZAT)?,
                BlockHeight::from(1),
                Script::from(transparent.script()).0.0,
            )],
        )
        .await?;
    let submitter = AcceptingSubmitter { network };
    let shielded = wallet
        .shield_transparent_funds(ShieldTransparentPlan::new(
            account_id,
            IdempotencyKey::try_from("shield-before-nu7")
                .map_err(|_| TestError::Fixture("invalid idempotency key"))?,
            zatoshis(SHIELDING_THRESHOLD_ZAT)?,
            &submitter,
        ))
        .await?;
    Ok(PendingShieldingWallet {
        wallet,
        account_id,
        chain,
        chain_handle,
        pending_tx_id: shielded.tx_id(),
        temp,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_signed_before_nu7_is_released_once_nu7_is_settled() -> Result<(), TestError> {
    let PendingShieldingWallet {
        wallet,
        account_id,
        chain,
        chain_handle,
        pending_tx_id,
        temp: _temp,
    } = wallet_with_pending_shielding(regtest_with_nu7_at_one_hundred_twenty()).await?;
    assert_eq!(
        pending_inputs_of(&wallet, account_id).await?,
        vec![pending_tx_id],
        "the shielding broadcast records its transparent input"
    );

    let mut events = wallet.observe();

    // The block after the judged height is still an NU6.3 block.
    sync_to(
        &wallet,
        &chain,
        &chain_handle,
        NU7_ACTIVATION_HEIGHT - 1 + 6,
        NU7_ACTIVATION_HEIGHT - 2,
    )
    .await?;
    assert!(unmineable_reports(&mut events).await.is_empty());
    assert_eq!(
        pending_inputs_of(&wallet, account_id).await?,
        vec![pending_tx_id]
    );

    // The settled tip reaches the last NU6.3 block, so the next block is an NU7 block.
    sync_to(
        &wallet,
        &chain,
        &chain_handle,
        NU7_ACTIVATION_HEIGHT - 1 + 6,
        NU7_ACTIVATION_HEIGHT - 1,
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

    // A later sync does not report it again.
    sync_to(
        &wallet,
        &chain,
        &chain_handle,
        NU7_ACTIVATION_HEIGHT + 10,
        NU7_ACTIVATION_HEIGHT + 10,
    )
    .await?;
    assert!(unmineable_reports(&mut events).await.is_empty());
    Ok(())
}

/// Accepts every transaction and echoes its canonical transaction ID, as a node does.
struct AcceptingSubmitter {
    network: Network,
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
        Ok(SubmitOutcome::Accepted { tx_id })
    }
}

async fn pending_inputs_of(
    wallet: &Wallet,
    account_id: zally_core::AccountId,
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
