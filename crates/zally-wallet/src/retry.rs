//! Retry policy and helper for wallet IO.
//!
//! `RetryPolicy` configures how the wallet retries transient, retryable failures from chain
//! reads, submissions, sealed-seed reads, and storage operations. Each error type carries a
//! per-variant `is_retryable()` posture; the helper [`with_retry`] consults that posture and
//! gives up on the first non-retryable error.

use std::future::Future;
use std::time::Duration;

use tokio::time::sleep;
use tracing::warn;

use zally_chain::{ChainSourceError, SubmitterError};
use zally_keys::SealingError;
use zally_pczt::PcztError;
use zally_storage::StorageError;

use crate::circuit_breaker::CircuitBreaker;
use crate::wallet_error::WalletError;

/// Retry policy applied to outbound wallet IO.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RetryPolicy {
    /// Maximum number of attempts including the initial call. `1` disables retries.
    pub max_attempts: u32,
    /// Backoff between the first retry and the second attempt, in milliseconds.
    pub initial_backoff_ms: u32,
    /// Cap on the backoff duration, in milliseconds.
    pub max_backoff_ms: u32,
    /// Multiplier applied to the prior backoff between successive retries.
    pub backoff_multiplier_x10: u32,
}

impl RetryPolicy {
    /// No retries: a single attempt, never sleeps. Failed calls surface immediately.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
            backoff_multiplier_x10: 10,
        }
    }

    /// `max_attempts` attempts with a fixed `delay_ms` between every retry.
    #[must_use]
    pub const fn linear(max_attempts: u32, delay_ms: u32) -> Self {
        Self {
            max_attempts,
            initial_backoff_ms: delay_ms,
            max_backoff_ms: delay_ms,
            backoff_multiplier_x10: 10,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 2_000,
            backoff_multiplier_x10: 20,
        }
    }
}

/// Posture introspection: did this error class earn a retry?
pub trait IsRetryable {
    /// `true` when the same call may succeed on retry.
    fn is_retryable(&self) -> bool;
}

impl IsRetryable for ChainSourceError {
    fn is_retryable(&self) -> bool {
        Self::is_retryable(self)
    }
}

impl IsRetryable for SubmitterError {
    fn is_retryable(&self) -> bool {
        Self::is_retryable(self)
    }
}

impl IsRetryable for StorageError {
    fn is_retryable(&self) -> bool {
        Self::is_retryable(self)
    }
}

impl IsRetryable for SealingError {
    fn is_retryable(&self) -> bool {
        Self::is_retryable(self)
    }
}

impl IsRetryable for PcztError {
    fn is_retryable(&self) -> bool {
        Self::is_retryable(self)
    }
}

impl IsRetryable for WalletError {
    fn is_retryable(&self) -> bool {
        Self::is_retryable(self)
    }
}

/// Retries `operation` up to `policy.max_attempts` times when `operation` returns a
/// retryable error. Non-retryable errors surface immediately; the policy never sleeps after
/// the final attempt.
///
/// `operation_label` is recorded in tracing breadcrumbs so operators can diagnose retry
/// storms by call site.
///
/// # Errors
///
/// Returns the last error from `operation` once retries are exhausted or `operation`
/// returns a non-retryable error.
pub async fn with_retry<T, E, F, Fut>(
    policy: RetryPolicy,
    operation_label: &'static str,
    mut operation: F,
) -> Result<T, E>
where
    E: IsRetryable + std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempts: u32 = 0;
    let mut delay_ms = policy.initial_backoff_ms;
    loop {
        attempts = attempts.saturating_add(1);
        match operation().await {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                if !err.is_retryable() || attempts >= policy.max_attempts {
                    return Err(err);
                }
                warn!(
                    target: "zally::retry",
                    event = "retry_attempt",
                    op = operation_label,
                    attempts,
                    delay_ms,
                    reason = %err,
                    "transient failure; backing off and retrying"
                );
                sleep(Duration::from_millis(u64::from(delay_ms))).await;
                let next = u64::from(delay_ms)
                    .saturating_mul(u64::from(policy.backoff_multiplier_x10))
                    / 10;
                delay_ms = u32::try_from(next.min(u64::from(policy.max_backoff_ms)))
                    .unwrap_or(policy.max_backoff_ms);
            }
        }
    }
}

/// Wraps [`with_retry`] with a [`CircuitBreaker`] check.
///
/// Returns [`WalletError::CircuitBroken`] immediately when the breaker is open. On call
/// completion, records success or failure on the breaker so subsequent calls reflect the
/// new state.
///
/// # Errors
///
/// Returns [`WalletError::CircuitBroken`] when the breaker is open. Otherwise returns
/// whatever the inner operation returns (mapped to [`WalletError`] by `lift_err`).
pub(crate) async fn with_breaker_and_retry<T, InnerError, F, Fut, LiftFn>(
    breaker: &CircuitBreaker,
    policy: RetryPolicy,
    operation_label: &'static str,
    operation: F,
    lift_err: LiftFn,
) -> Result<T, WalletError>
where
    InnerError: IsRetryable + std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, InnerError>>,
    LiftFn: FnOnce(InnerError) -> WalletError,
{
    if !breaker.allow_call() {
        return Err(WalletError::CircuitBroken {
            operation: operation_label,
        });
    }
    match with_retry(policy, operation_label, operation).await {
        Ok(outcome) => {
            breaker.record_success();
            Ok(outcome)
        }
        Err(err) => {
            if err.is_retryable() {
                breaker.record_failure();
            } else {
                breaker.record_success();
            }
            Err(lift_err(err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Debug, thiserror::Error)]
    enum FakeError {
        #[error("retryable: {0}")]
        Retryable(String),
        #[error("permanent: {0}")]
        Permanent(String),
    }

    impl IsRetryable for FakeError {
        fn is_retryable(&self) -> bool {
            matches!(self, Self::Retryable(_))
        }
    }

    #[tokio::test]
    async fn with_retry_returns_immediately_on_success() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter);
        let outcome: Result<u32, FakeError> =
            with_retry(RetryPolicy::default(), "test", move || {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, FakeError>(42)
                }
            })
            .await;
        assert!(matches!(outcome, Ok(42)));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn with_retry_retries_retryable_errors_then_succeeds() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter);
        let policy = RetryPolicy {
            max_attempts: 5,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
            backoff_multiplier_x10: 10,
        };
        let outcome: Result<u32, FakeError> = with_retry(policy, "test", move || {
            let c = Arc::clone(&c);
            async move {
                let attempt = c.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err(FakeError::Retryable("flaky".into()))
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert!(matches!(outcome, Ok(7)));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "two failures then success"
        );
    }

    #[tokio::test]
    async fn with_retry_does_not_retry_permanent_errors() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter);
        let outcome: Result<u32, FakeError> =
            with_retry(RetryPolicy::default(), "test", move || {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err::<u32, _>(FakeError::Permanent("config".into()))
                }
            })
            .await;
        assert!(matches!(outcome, Err(FakeError::Permanent(_))));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn with_retry_gives_up_after_max_attempts() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter);
        let policy = RetryPolicy::linear(3, 1);
        let outcome: Result<u32, FakeError> = with_retry(policy, "test", move || {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<u32, _>(FakeError::Retryable("flaky".into()))
            }
        })
        .await;
        assert!(matches!(outcome, Err(FakeError::Retryable(_))));
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }
}
