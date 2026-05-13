//! Regression: `Wallet::send_payment` returns the prior `tx_id` and skips the submitter
//! when the supplied `IdempotencyKey` is already recorded in the ledger.

use zally_core::{
    BlockHeight, IdempotencyKey, IdempotencyKeyError, Network, PaymentRecipient, TxId, Zatoshis,
    ZatoshisError,
};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions, StorageError, WalletStorage};
use zally_testkit::{MockSubmitter, TempWalletPath};
use zally_wallet::{SendPaymentPlan, Wallet, WalletError};

#[tokio::test]
async fn send_payment_short_circuits_on_known_idempotency_key() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    storage.open_or_create().await?;

    let known_key = IdempotencyKey::try_from("invoice-deadbeef-1")?;
    let prior_tx_id = TxId::from_bytes([0xCC_u8; 32]);
    storage
        .record_idempotent_submission(known_key.clone(), prior_tx_id)
        .await?;

    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

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
        outcome.tx_id, prior_tx_id,
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
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("idempotency key error: {0}")]
    Key(#[from] IdempotencyKeyError),
    #[error("zatoshis error: {0}")]
    Zatoshis(#[from] ZatoshisError),
}
