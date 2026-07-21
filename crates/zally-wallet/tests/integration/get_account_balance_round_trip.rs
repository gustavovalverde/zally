//! Regression: `Wallet::get_account_balance` reports network-tagged per-pool zeros for a
//! fresh wallet and tracks the observed tip after the first sync.
//!
//! The mock chain source returns no compact blocks, so the test cannot exercise an account
//! with real Sapling, Orchard, Ironwood, and transparent UTXOs (that case lives under the T3 live
//! profile). What is exercised here: the wrapping pattern, the network tag, the
//! `as_of_height` lifecycle, the read-only invariant, and the unknown-account error.

use uuid::Uuid;
use zally_chain::ChainSource as _;
use zally_core::{AccountId, BlockHeight, Zatoshis};
use zally_testkit::MockChainSource;
use zally_wallet::WalletError;

use super::fixtures::{TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn get_account_balance_returns_zeros_on_fresh_wallet() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let balance = wallet.get_account_balance(account_id).await?;

    assert_eq!(balance.network, network);
    assert_eq!(balance.sapling_zat, Zatoshis::zero());
    assert_eq!(balance.orchard_zat, Zatoshis::zero());
    assert_eq!(balance.ironwood_zat, Zatoshis::zero());
    assert_eq!(balance.transparent_mature_zat, Zatoshis::zero());
    assert_eq!(balance.transparent_immature_zat, Zatoshis::zero());
    assert!(
        balance.as_of_height.is_none(),
        "fresh wallet has no observed tip"
    );
    assert_eq!(balance.shielded_zat(), Zatoshis::zero());
    assert_eq!(balance.transparent_zat(), Zatoshis::zero());
    assert_eq!(balance.total_zat(), Zatoshis::zero());
    Ok(())
}

#[tokio::test]
async fn get_account_balance_anchors_to_visible_tip_after_sync() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(200));
    wallet.sync(&chain).await?;
    let tip = chain
        .current_epoch()
        .await
        .map_err(|err| TestError::Chain {
            reason: err.to_string(),
        })?
        .settled_tip()
        .height;
    assert_eq!(tip.as_u32(), 200, "mock chain tip must be set");

    let balance = wallet.get_account_balance(account_id).await?;
    assert_eq!(balance.as_of_height, Some(BlockHeight::from(200)));
    assert_eq!(balance.total_zat(), Zatoshis::zero());
    Ok(())
}

#[tokio::test]
async fn get_account_balance_is_idempotent() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;

    let first = wallet.get_account_balance(account_id).await?;
    let second = wallet.get_account_balance(account_id).await?;
    assert_eq!(
        first, second,
        "balance read must be a pure snapshot, no state change between calls"
    );
    Ok(())
}

#[tokio::test]
async fn get_account_balance_unknown_account_returns_account_not_found() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;

    let unknown = AccountId::from_uuid(Uuid::nil());
    let outcome = wallet.get_account_balance(unknown).await;
    match outcome {
        Err(WalletError::AccountNotFound) => Ok(()),
        Err(other) => Err(TestError::Unexpected {
            reason: format!("expected AccountNotFound, got {other:?}"),
        }),
        Ok(balance) => Err(TestError::Unexpected {
            reason: format!("expected error, got snapshot {balance:?}"),
        }),
    }
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] super::fixtures::TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("chain source error: {reason}")]
    Chain { reason: String },
    #[error("unexpected test outcome: {reason}")]
    Unexpected { reason: String },
}
