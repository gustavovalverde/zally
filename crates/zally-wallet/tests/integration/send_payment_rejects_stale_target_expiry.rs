//! `Wallet::send_payment` rejects a `target_expiry_height` at or below the wallet's
//! observed chain tip with [`WalletError::TargetExpiryStale`].
//!
//! The wallet must not sign bytes whose `expiry_height` is already past, because
//! consensus would reject the broadcast immediately. The check fires before any
//! PCZT or proving work runs.

use zally_core::{
    BlockHeight, IdempotencyKey, IdempotencyKeyError, PaymentRecipient, Zatoshis, ZatoshisError,
};
use zally_testkit::MockSubmitter;
use zally_wallet::{SendPaymentPlan, WalletError};

use super::fixtures::{TestWalletFixture, create_test_wallet};

#[tokio::test]
#[allow(
    clippy::panic,
    reason = "the test asserts on the exact error shape; panic makes the failing variant readable"
)]
async fn send_payment_rejects_stale_target_expiry() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();

    // A freshly created wallet has not observed any tip yet, so the wallet's chain tip
    // defaults to height 0. A target of 0 lands at the floor, which the wallet must
    // reject as stale.
    let stale_target = BlockHeight::from(0);
    let recipient_ua = wallet.derive_next_address(account_id).await?;
    let encoded = recipient_ua.encode(&network.to_parameters());
    let submitter = MockSubmitter::accepting(network);
    let submitter_handle = submitter.handle();

    let plan = SendPaymentPlan::conventional(
        account_id,
        IdempotencyKey::try_from("stale-target-expiry-key")?,
        PaymentRecipient::UnifiedAddress { encoded, network },
        Zatoshis::try_from(10_000_u64)?,
        &submitter,
    )
    .with_target_expiry_height(stale_target);

    let outcome = wallet.send_payment(plan).await;
    match outcome {
        Err(WalletError::TargetExpiryStale { target, chain_tip }) => {
            assert_eq!(
                target, stale_target,
                "the rejection must echo the caller's target"
            );
            assert_eq!(
                chain_tip,
                BlockHeight::from(0),
                "a fresh wallet's observed tip is the floor"
            );
        }
        other => panic!("expected TargetExpiryStale, got {other:?}"),
    }
    assert_eq!(
        submitter_handle.submission_count(),
        0,
        "the submitter must not run when the wallet rejects the plan up front"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] super::fixtures::TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("idempotency key error: {0}")]
    Key(#[from] IdempotencyKeyError),
    #[error("zatoshis error: {0}")]
    Zatoshis(#[from] ZatoshisError),
}
