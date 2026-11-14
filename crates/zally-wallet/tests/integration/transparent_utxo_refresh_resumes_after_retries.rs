//! Regression: one `Wallet::refresh_transparent_utxos` call must outlast a chain source that
//! briefly stops serving or rotates its epoch mid-walk.
//!
//! Zinder answers `ServiceNotReady` (retryable) during a readiness gap and rotates its epoch
//! on canonical appends, so a walk over dozens of receivers can lose its pin before it
//! finishes. A retry re-pins and resumes at the receiver that failed rather than restarting
//! the walk, and a walk whose retries run out reports how far it got.

use zally_chain::ChainSourceError;
use zally_core::{BlockHeight, FailurePosture};
use zally_testkit::MockChainSource;
use zally_wallet::{RetryPolicy, WalletError};

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

/// Successful receiver reads before the mock rotates its epoch mid-walk.
const READS_BEFORE_ROTATION: u32 = 5;

#[tokio::test]
async fn refresh_resumes_at_the_failed_receiver_after_a_rotation() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    wallet.set_transparent_utxo_refresh_retry_policy(RetryPolicy::linear(3, 1));
    let chain = MockChainSource::new(wallet.network());
    let chain_handle = chain.handle();
    chain_handle.advance_tip(BlockHeight::from(50));
    chain_handle.expire_epoch_after_transparent_reads(READS_BEFORE_ROTATION);

    let pins_before = chain_handle.acquired_epoch_ids().len();
    let reads_before = chain_handle.artifact_epoch_ids().len();
    let outcome = wallet.refresh_transparent_utxos(&chain).await?;

    let rotation_index = usize::try_from(READS_BEFORE_ROTATION).unwrap_or(usize::MAX);
    assert!(
        outcome.receivers_visited > u64::try_from(rotation_index).unwrap_or(u64::MAX),
        "the fixture wallet must carry more receivers than the rotation point, visited {}",
        outcome.receivers_visited
    );
    let pins = &chain_handle.acquired_epoch_ids()[pins_before..];
    assert_eq!(pins.len(), 2, "one pin per attempt, got {pins:?}");
    let reads = &chain_handle.artifact_epoch_ids()[reads_before..];
    assert_eq!(
        u64::try_from(reads.len()).unwrap_or(u64::MAX),
        outcome.receivers_visited.saturating_add(1),
        "only the receiver whose read lost the pin is read twice"
    );
    assert!(
        reads[..=rotation_index].iter().all(|id| *id == pins[0])
            && reads[rotation_index + 1..].iter().all(|id| *id == pins[1]),
        "the walk must read the old pin up to the rotation and the fresh pin after it, got {reads:?}"
    );
    Ok(())
}

#[tokio::test]
async fn refresh_outlasts_a_source_that_is_briefly_not_ready() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    wallet.set_transparent_utxo_refresh_retry_policy(RetryPolicy::linear(4, 1));
    let chain = MockChainSource::new(wallet.network());
    let chain_handle = chain.handle();
    chain_handle.advance_tip(BlockHeight::from(50));
    chain_handle.fail_transparent_utxos_next(3, || ChainSourceError::Unavailable {
        reason: "service is not ready to accept new traffic".to_owned(),
    });

    let outcome = wallet.refresh_transparent_utxos(&chain).await?;

    assert!(outcome.receivers_visited > 0);
    assert_eq!(chain_handle.failures_consumed(), 3);
    Ok(())
}

#[tokio::test]
async fn exhausted_refresh_reports_its_attempts_and_cause() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    wallet.set_transparent_utxo_refresh_retry_policy(RetryPolicy::linear(3, 1));
    let chain = MockChainSource::new(wallet.network());
    let chain_handle = chain.handle();
    chain_handle.advance_tip(BlockHeight::from(50));
    let receiver_count = wallet
        .refresh_transparent_utxos(&chain)
        .await?
        .receivers_visited;
    chain_handle.fail_transparent_utxos_next(10, || ChainSourceError::Unavailable {
        reason: "service is not ready to accept new traffic".to_owned(),
    });

    let Err(error) = wallet.refresh_transparent_utxos(&chain).await else {
        return Err(TestError::UnexpectedSuccess);
    };

    assert_eq!(error.posture(), FailurePosture::Retryable);
    assert!(
        matches!(
            &error,
            WalletError::TransparentUtxoRefreshIncomplete {
                receivers_refreshed: 0,
                receivers,
                attempts: 3,
                source,
            } if *receivers == receiver_count
                && matches!(
                    source.as_ref(),
                    WalletError::ChainSource(ChainSourceError::Unavailable { .. })
                )
        ),
        "the exhausted walk must report its attempts and cause, got {error:?}"
    );
    assert_eq!(chain_handle.failures_consumed(), 3);
    Ok(())
}

#[tokio::test]
async fn interrupted_refresh_reports_the_receivers_it_refreshed() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: _account_id,
    } = create_test_wallet().await?;
    wallet.set_transparent_utxo_refresh_retry_policy(RetryPolicy::none());
    let chain = MockChainSource::new(wallet.network());
    let chain_handle = chain.handle();
    chain_handle.advance_tip(BlockHeight::from(50));
    chain_handle.expire_epoch_after_transparent_reads(READS_BEFORE_ROTATION);

    let Err(error) = wallet.refresh_transparent_utxos(&chain).await else {
        return Err(TestError::UnexpectedSuccess);
    };

    assert_eq!(error.posture(), FailurePosture::Restartable);
    assert!(
        matches!(
            &error,
            WalletError::TransparentUtxoRefreshIncomplete {
                receivers_refreshed,
                attempts: 1,
                source,
                ..
            } if *receivers_refreshed == u64::from(READS_BEFORE_ROTATION)
                && matches!(
                    source.as_ref(),
                    WalletError::ChainSource(ChainSourceError::ChainEpochPinUnavailable)
                )
        ),
        "the interrupted walk must report the receivers it refreshed, got {error:?}"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("the refresh succeeded where it had to run out of retries")]
    UnexpectedSuccess,
}
