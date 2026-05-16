//! Regression: the wallet's circuit breaker only trips on retryable failures.
//!
//! Consecutive [`FailurePosture::Retryable`] failures advance the breaker counter until it
//! opens; `Wallet::capabilities()` reflects `CircuitBroken` while open. Operator-action
//! failures do not advance the counter since they are not symptoms of a flaky backend.

use zally_chain::ChainSourceError;
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{MockChainSource, TempWalletPath};
use zally_wallet::{Capability, CircuitBreakerState, RetryPolicy, Wallet, WalletError};

#[tokio::test]
async fn circuit_breaker_opens_after_threshold_retryable_failures() -> Result<(), TestError> {
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
    wallet.set_retry_policy(RetryPolicy::none());

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain
        .handle()
        .fail_chain_tip_next(20, || ChainSourceError::Unavailable {
            reason: "simulated outage".into(),
        });

    assert!(
        matches!(
            wallet.circuit_breaker_state(),
            CircuitBreakerState::Closed { .. }
        ),
        "breaker must start closed"
    );

    for _ in 0..5 {
        let _ = wallet.sync(&chain).await;
    }

    assert!(
        matches!(wallet.circuit_breaker_state(), CircuitBreakerState::Open),
        "breaker must be open after the threshold failures, got {:?}",
        wallet.circuit_breaker_state()
    );
    assert!(
        wallet
            .capabilities()
            .features
            .contains(&Capability::CircuitBroken),
        "Capability::CircuitBroken must appear on the live snapshot"
    );

    let outcome = wallet.sync(&chain).await;
    assert!(
        matches!(outcome, Err(WalletError::CircuitBroken { .. })),
        "open breaker must short-circuit subsequent calls, got {outcome:?}"
    );
    Ok(())
}

#[tokio::test]
async fn circuit_breaker_does_not_trip_on_requires_operator_failures() -> Result<(), TestError> {
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
    wallet.set_retry_policy(RetryPolicy::none());

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain
        .handle()
        .fail_chain_tip_next(20, || ChainSourceError::MalformedCompactBlock {
            block_height: BlockHeight::from(10),
            reason: "synthetic upstream malformed".into(),
        });

    for _ in 0..10 {
        let _ = wallet.sync(&chain).await;
    }

    assert!(
        matches!(
            wallet.circuit_breaker_state(),
            CircuitBreakerState::Closed { .. }
        ),
        "operator-action failures must not trip the breaker, got {:?}",
        wallet.circuit_breaker_state()
    );
    assert!(
        !wallet
            .capabilities()
            .features
            .contains(&Capability::CircuitBroken),
        "Capability::CircuitBroken must not appear while the breaker is closed"
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
