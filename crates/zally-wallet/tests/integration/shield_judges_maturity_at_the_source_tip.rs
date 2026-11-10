//! Shielding judges transparent maturity at the lower of the wallet's chain tip and the
//! chain source's latest visible tip.
//!
//! A source restarted onto a shorter branch serves a tip below the one the wallet recorded,
//! and the transparent-UTXO refresh can then record a coinbase mined on that branch. Counted
//! against the wallet's stale tip it looks mature, but the node mines the shielding
//! transaction above its own tip and rejects the immature spend. The wallet leaves such an
//! output out until it is mature at the source's tip as well.

use zally_core::{BlockHeight, IdempotencyKey, TxId};
use zally_wallet::WalletError;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BranchId;

use super::reorg_fixtures::{
    ChainWallet, FixtureError, chain_wallet, record_transparent_utxo_at, shield_into_sapling,
    sync_to_tip, transaction_id_submitter,
};

const WALLET_TIP_HEIGHT: u32 = 300;
/// A source tip far enough below the wallet's that an output one block under it would count
/// 100 confirmations against the wallet's tip.
const SOURCE_TIP_HEIGHT: u32 = 200;
const COINBASE_HEIGHT: u32 = SOURCE_TIP_HEIGHT - 1;
const COINBASE_TX_ID: [u8; 32] = [0x3c; 32];

/// A wallet scanned to [`WALLET_TIP_HEIGHT`] whose chain source then fell back to
/// [`SOURCE_TIP_HEIGHT`], holding a coinbase the refresh recorded from the source's branch.
async fn wallet_ahead_of_its_source() -> Result<ChainWallet, FixtureError> {
    let base = chain_wallet(WALLET_TIP_HEIGHT - 100).await?;
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    base.chain_handle
        .advance_tip(BlockHeight::from(WALLET_TIP_HEIGHT));
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;

    move_source_tip(&base, SOURCE_TIP_HEIGHT).await?;
    record_transparent_utxo_at(&base, COINBASE_TX_ID, COINBASE_HEIGHT).await?;
    Ok(base)
}

/// Moves the source's tip without announcing it, and lets the wallet read it the way the
/// transparent-UTXO refresh does.
async fn move_source_tip(base: &ChainWallet, tip_height: u32) -> Result<(), FixtureError> {
    let tip = BlockHeight::from(tip_height);
    if !base.chain_handle.set_chain_tips(tip, tip) {
        return Err(FixtureError::Fixture("the mock refused the source tip"));
    }
    base.wallet
        .refresh_transparent_utxos(base.chain.as_ref())
        .await?;
    Ok(())
}

fn shielding_key(key: &str) -> Result<IdempotencyKey, FixtureError> {
    IdempotencyKey::try_from(key).map_err(|_| FixtureError::Fixture("idempotency key"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_coinbase_immature_at_the_source_tip_is_not_shielded() -> Result<(), FixtureError> {
    let base = wallet_ahead_of_its_source().await?;
    let submitter = transaction_id_submitter(base.network);

    let refused =
        shield_into_sapling(&base, shielding_key("shield-at-source-tip")?, &submitter).await;

    assert!(
        matches!(
            refused,
            Err(FixtureError::Wallet(
                WalletError::InsufficientBalance { .. }
            ))
        ),
        "a coinbase two blocks under the source's tip must not count as mature, got {refused:?}"
    );
    assert_eq!(
        submitter.handle().submission_count(),
        0,
        "nothing reaches the node while the coinbase is immature at its tip"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_coinbase_is_shielded_once_the_source_tip_matures_it() -> Result<(), FixtureError> {
    let base = wallet_ahead_of_its_source().await?;
    let submitter = transaction_id_submitter(base.network);
    let key = shielding_key("shield-after-source-recovers")?;
    let refused = shield_into_sapling(&base, key.clone(), &submitter).await;
    assert!(
        matches!(
            refused,
            Err(FixtureError::Wallet(
                WalletError::InsufficientBalance { .. }
            ))
        ),
        "the coinbase is immature at the source's tip, got {refused:?}"
    );

    move_source_tip(&base, WALLET_TIP_HEIGHT).await?;
    let outcome = shield_into_sapling(&base, key, &submitter).await?;

    let submissions = submitter.handle().submitted_bytes();
    let [raw] = submissions.as_slice() else {
        return Err(FixtureError::Fixture(
            "expected exactly one shielding submission",
        ));
    };
    let shielding = Transaction::read(raw.as_slice(), BranchId::Nu7)?;
    assert_eq!(
        TxId::from_bytes(*shielding.txid().as_ref()),
        outcome.tx_id()
    );
    let spends_coinbase = shielding.transparent_bundle().is_some_and(|bundle| {
        bundle
            .vin
            .iter()
            .any(|input| *input.prevout().hash() == COINBASE_TX_ID)
    });
    assert!(
        spends_coinbase,
        "the shielding must spend the coinbase once it is mature at the source's tip"
    );
    Ok(())
}
