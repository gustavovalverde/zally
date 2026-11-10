//! Regression: `Wallet::send_payment` returns the prior `tx_id` for a recorded key.
//!
//! It builds nothing when the supplied `IdempotencyKey` is already recorded in the ledger. The
//! wallet holds no bytes for this prior transaction, so it has nothing to submit again either.

use zally_core::{
    IdempotencyKey, IdempotencyKeyError, PaymentRecipient, TxId, Zatoshis, ZatoshisError,
};
use zally_storage::{StorageError, WalletStorage};
use zally_testkit::MockSubmitter;
use zally_wallet::{SendPaymentPlan, WalletError};

use super::fixtures::{TestWalletFixture, create_test_wallet_with_storage};

#[tokio::test]
async fn send_payment_short_circuits_on_known_idempotency_key() -> Result<(), TestError> {
    let (
        TestWalletFixture {
            temp: _temp,
            wallet,
            account_id,
        },
        storage,
    ) = create_test_wallet_with_storage().await?;
    let network = wallet.network();

    let known_key = IdempotencyKey::try_from("invoice-deadbeef-1")?;
    let prior_tx_id = TxId::from_bytes([0xCC_u8; 32]);
    storage
        .record_idempotent_submission(known_key.clone(), prior_tx_id)
        .await?;

    let recipient_ua = wallet.derive_next_address(account_id).await?;
    let encoded = recipient_ua.encode(&network.to_parameters());
    let submitter = MockSubmitter::accepting(network);
    let submitter_handle = submitter.handle();

    let plan = SendPaymentPlan::conventional(
        account_id,
        known_key,
        PaymentRecipient::UnifiedAddress { encoded, network },
        Zatoshis::try_from(10_000_u64)?,
        &submitter,
    );
    let outcome = wallet.send_payment(plan).await?;

    assert_eq!(
        outcome.tx_id(),
        prior_tx_id,
        "ledger hit must return the previously-recorded tx_id"
    );
    assert_eq!(
        submitter_handle.submission_count(),
        0,
        "the submitter must not be called when the ledger hit short-circuits"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] super::fixtures::TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("idempotency key error: {0}")]
    Key(#[from] IdempotencyKeyError),
    #[error("zatoshis error: {0}")]
    Zatoshis(#[from] ZatoshisError),
}
