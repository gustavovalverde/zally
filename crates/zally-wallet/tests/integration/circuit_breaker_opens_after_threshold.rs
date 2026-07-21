//! Regression: the wallet's circuit breaker only trips on retryable failures.
//!
//! Consecutive [`FailurePosture::Retryable`] failures advance the breaker counter until it
//! opens; `Wallet::capabilities()` reflects `CircuitBroken` while open. Operator-action
//! failures do not advance the counter since they are not symptoms of a flaky backend.

use zally_chain::ChainSourceError;
use zally_core::BlockHeight;
use zally_testkit::MockChainSource;
use zally_wallet::{Capability, CircuitBreakerState, RetryPolicy, WalletError};

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn circuit_breaker_opens_after_threshold_retryable_failures() -> Result<(), TestWalletError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::none());

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain
        .handle()
        .fail_current_epoch_next(20, || ChainSourceError::Unavailable {
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
async fn circuit_breaker_does_not_trip_on_requires_operator_failures() -> Result<(), TestWalletError>
{
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    wallet.set_retry_policy(RetryPolicy::none());

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(50));
    chain
        .handle()
        .fail_current_epoch_next(20, || ChainSourceError::MalformedCompactBlock {
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
