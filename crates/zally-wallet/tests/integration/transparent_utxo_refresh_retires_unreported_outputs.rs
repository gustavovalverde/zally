//! The transparent-UTXO refresh retires a recorded output its chain source stops reporting.
//!
//! A reorg can replace the block that mined a coinbase after the refresh recorded it, and a
//! block below the wallet's scanned height can be replaced without the block scan noticing
//! a transparent-only transaction. The source's complete unspent set for the receiver then
//! omits the output. A shielding that still spent it would be rejected for a missing input,
//! so the refresh takes it out of every balance and proposal, and records it again if the
//! source reports it again.

use zally_core::{BlockHeight, IdempotencyKey, TxId};
use zally_wallet::WalletError;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BranchId;

use super::reorg_fixtures::{
    ChainWallet, FixtureError, chain_wallet, record_transparent_utxo_at, shield_into_sapling,
    sync_to_tip, transaction_id_submitter,
};

const TIP_HEIGHT: u32 = 300;
const COINBASE_HEIGHT: u32 = 150;
const COINBASE_TX_ID: [u8; 32] = [0x4e; 32];

/// A wallet scanned to [`TIP_HEIGHT`] that recorded a mature coinbase at [`COINBASE_HEIGHT`],
/// on a source whose epoch's settled tip is at the visible tip, above the coinbase.
async fn wallet_with_a_recorded_coinbase() -> Result<(ChainWallet, Vec<u8>), FixtureError> {
    let base = chain_wallet(TIP_HEIGHT - 100).await?;
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    base.chain_handle.advance_tip(BlockHeight::from(TIP_HEIGHT));
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    let script = record_transparent_utxo_at(&base, COINBASE_TX_ID, COINBASE_HEIGHT).await?;
    Ok((base, script))
}

fn shielding_key(key: &str) -> Result<IdempotencyKey, FixtureError> {
    IdempotencyKey::try_from(key).map_err(|_| FixtureError::Fixture("idempotency key"))
}

fn spends_coinbase(raw: &[u8]) -> Result<bool, FixtureError> {
    let shielding = Transaction::read(raw, BranchId::Nu7)?;
    Ok(shielding.transparent_bundle().is_some_and(|bundle| {
        bundle
            .vin
            .iter()
            .any(|input| *input.prevout().hash() == COINBASE_TX_ID)
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_output_the_source_stops_reporting_is_not_shielded() -> Result<(), FixtureError> {
    let (base, script) = wallet_with_a_recorded_coinbase().await?;
    let balance = base.wallet.get_account_balance(base.account_id).await?;
    assert!(
        balance.transparent_mature_zat.as_u64() > 0,
        "the recorded coinbase counts while the source reports it: {balance:?}"
    );

    base.chain_handle
        .serve_transparent_utxos(script, Vec::new());
    let refreshed = base
        .wallet
        .refresh_transparent_utxos(base.chain.as_ref())
        .await?;
    assert_eq!(
        refreshed.utxos_retired, 1,
        "the refresh retires the output its source no longer reports"
    );

    let balance = base.wallet.get_account_balance(base.account_id).await?;
    assert_eq!(
        balance.transparent_mature_zat.as_u64(),
        0,
        "a retired output leaves the balance: {balance:?}"
    );
    let submitter = transaction_id_submitter(base.network);
    let refused = shield_into_sapling(&base, shielding_key("shield-retired")?, &submitter).await;
    assert!(
        matches!(
            refused,
            Err(FixtureError::Wallet(
                WalletError::InsufficientBalance { .. }
            ))
        ),
        "a shielding must not select a retired output, got {refused:?}"
    );
    assert_eq!(
        submitter.handle().submission_count(),
        0,
        "nothing reaches the node for a retired output"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_output_the_source_reports_again_is_shielded() -> Result<(), FixtureError> {
    let (base, script) = wallet_with_a_recorded_coinbase().await?;
    base.chain_handle
        .serve_transparent_utxos(script.clone(), Vec::new());
    base.wallet
        .refresh_transparent_utxos(base.chain.as_ref())
        .await?;

    base.chain_handle.serve_transparent_utxos(
        script.clone(),
        vec![zally_chain::TransparentUtxo {
            tx_id: TxId::from_bytes(COINBASE_TX_ID),
            output_index: 0,
            value_zat: zally_core::Zatoshis::try_from(1_000_000_u64)
                .map_err(|_| FixtureError::Fixture("coinbase value"))?,
            confirmed_at_height: BlockHeight::from(COINBASE_HEIGHT),
            script_pub_key_bytes: script,
        }],
    );
    let refreshed = base
        .wallet
        .refresh_transparent_utxos(base.chain.as_ref())
        .await?;
    assert_eq!(refreshed.utxos_retired, 0);

    let submitter = transaction_id_submitter(base.network);
    let outcome = shield_into_sapling(&base, shielding_key("shield-restored")?, &submitter).await?;
    let submissions = submitter.handle().submitted_bytes();
    let [raw] = submissions.as_slice() else {
        return Err(FixtureError::Fixture(
            "expected exactly one shielding submission",
        ));
    };
    assert_eq!(
        TxId::from_bytes(
            *Transaction::read(raw.as_slice(), BranchId::Nu7)?
                .txid()
                .as_ref()
        ),
        outcome.tx_id()
    );
    assert!(
        spends_coinbase(raw)?,
        "an output the source reports again is spendable again"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_output_above_the_source_tip_is_kept() -> Result<(), FixtureError> {
    let (base, script) = wallet_with_a_recorded_coinbase().await?;
    let lowered_tip = BlockHeight::from(COINBASE_HEIGHT - 1);
    if !base.chain_handle.set_chain_tips(lowered_tip, lowered_tip) {
        return Err(FixtureError::Fixture("the mock refused the lowered tip"));
    }
    base.chain_handle
        .serve_transparent_utxos(script, Vec::new());

    let refreshed = base
        .wallet
        .refresh_transparent_utxos(base.chain.as_ref())
        .await?;

    assert_eq!(
        refreshed.utxos_retired, 0,
        "an epoch whose visible tip is below the output cannot judge it"
    );
    Ok(())
}
