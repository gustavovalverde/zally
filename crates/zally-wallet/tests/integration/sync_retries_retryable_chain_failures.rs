//! Regression: `Wallet::sync` retries `chain.chain_tip()` for retryable failures.
//!
//! Retries continue until the call succeeds or the policy ceiling is reached. Operator-
//! action and not-retryable failures surface immediately without burning the retry budget.

use zally_chain::ChainSourceError;
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{MockChainSource, TempWalletPath};
use zally_wallet::{RetryPolicy, Wallet, WalletError};

#[tokio::test]
async fn sync_retries_until_chain_tip_recovers() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, _account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;
    wallet.set_retry_policy(RetryPolicy::linear(4, 1));

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain
        .handle()
        .fail_chain_tip_next(2, || ChainSourceError::Unavailable {
            reason: "simulated upstream stall".into(),
        });

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
async fn sync_does_not_retry_operator_action_chain_failures() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, _account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;
    wallet.set_retry_policy(RetryPolicy::linear(4, 1));

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain
        .handle()
        .fail_chain_tip_next(1, || ChainSourceError::MalformedCompactBlock {
            block_height: BlockHeight::from(10),
            reason: "synthetic decode failure".into(),
        });

    let outcome = wallet.sync(&chain).await;
    assert!(
        matches!(outcome, Err(WalletError::ChainSource(_))),
        "operator-action error must surface, got {outcome:?}"
    );
    assert_eq!(
        chain.handle().failures_consumed(),
        1,
        "operator-action failures must consume one and only one queued error"
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
