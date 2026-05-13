//! Regression: `Wallet::sync` retries `chain.chain_tip()` for retryable failures (per the
//! per-error `is_retryable()` posture) until it either succeeds or hits the policy ceiling.

use zally_chain::ChainSourceError;
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{MockChainSource, TempWalletPath};
use zally_wallet::{RetryPolicy, Wallet, WalletError};

#[tokio::test]
async fn sync_retries_until_chain_tip_recovers() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let (wallet, _account_id, _mnemonic) =
        Wallet::create(network, sealing, storage, BlockHeight::from(1)).await?;
    wallet.set_retry_policy(RetryPolicy::linear(4, 1));

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    // Two transient failures, then chain_tip recovers on the third call.
    chain.handle().fail_chain_tip_next(
        2,
        ChainSourceError::Unavailable {
            reason: "simulated upstream stall".into(),
        },
    );

    let outcome = wallet.sync(&chain).await?;
    assert_eq!(outcome.scanned_to_height.as_u32(), 50);
    assert_eq!(
        chain.handle().failures_consumed(),
        2,
        "mock must have absorbed the two injected failures before succeeding"
    );
    Ok(())
}

#[tokio::test]
async fn sync_does_not_retry_permanent_chain_failures() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let (wallet, _account_id, _mnemonic) =
        Wallet::create(network, sealing, storage, BlockHeight::from(1)).await?;
    wallet.set_retry_policy(RetryPolicy::linear(4, 1));

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain.handle().fail_chain_tip_next(
        1,
        ChainSourceError::MalformedCompactBlock {
            block_height: BlockHeight::from(10),
            reason: "synthetic decode failure".into(),
        },
    );

    let outcome = wallet.sync(&chain).await;
    assert!(
        matches!(outcome, Err(WalletError::ChainSource { .. })),
        "non-retryable error must surface, got {outcome:?}"
    );
    assert_eq!(
        chain.handle().failures_consumed(),
        1,
        "permanent failures must consume one and only one queued error"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
