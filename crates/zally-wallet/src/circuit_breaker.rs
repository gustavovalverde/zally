//! Circuit breaker for wallet IO.
//!
//! Tracks consecutive retryable failures and trips open when they cross a threshold. While
//! open, subsequent calls short-circuit with [`crate::WalletError::CircuitBroken`] until the
//! cooldown elapses; the next call enters a half-open probe. A successful probe re-closes
//! the breaker; another failure re-opens it for another cooldown window.

use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Configurable circuit breaker for the wallet's IO boundary.
#[derive(Debug)]
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: Mutex<CircuitState>,
}

/// Configuration for [`CircuitBreaker`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CircuitBreakerConfig {
    /// Number of consecutive retryable failures that trip the breaker open.
    pub failure_threshold: u32,
    /// How long the breaker stays open before allowing a half-open probe.
    pub cooldown: Duration,
}

impl CircuitBreakerConfig {
    /// A configuration that never trips (`u32::MAX` threshold). Useful for tests that only
    /// exercise the retry path.
    #[must_use]
    pub const fn never_trip() -> Self {
        Self {
            failure_threshold: u32::MAX,
            cooldown: Duration::from_secs(0),
        }
    }
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown: Duration::from_secs(60),
        }
    }
}

/// Runtime state of a [`CircuitBreaker`]. Exposed via [`CircuitBreaker::state`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum CircuitBreakerState {
    /// All calls pass through. The internal counter tracks consecutive recent failures.
    Closed {
        /// Consecutive failures observed since the last success.
        consecutive_failures: u32,
    },
    /// Calls short-circuit until the cooldown elapses.
    Open,
    /// One probe call is allowed. Success re-closes the breaker; failure re-opens it.
    HalfOpen,
}

#[derive(Debug)]
enum CircuitState {
    Closed { consecutive_failures: u32 },
    Open { opened_at: Instant },
    HalfOpen,
}

impl CircuitBreaker {
    /// Constructs a breaker with `config`.
    #[must_use]
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: Mutex::new(CircuitState::Closed {
                consecutive_failures: 0,
            }),
        }
    }

    /// Returns the active configuration.
    #[must_use]
    pub fn config(&self) -> CircuitBreakerConfig {
        self.config
    }

    /// Returns the current breaker state.
    #[must_use]
    pub fn state(&self) -> CircuitBreakerState {
        match &*self.state.lock() {
            CircuitState::Closed {
                consecutive_failures,
            } => CircuitBreakerState::Closed {
                consecutive_failures: *consecutive_failures,
            },
            CircuitState::Open { .. } => CircuitBreakerState::Open,
            CircuitState::HalfOpen => CircuitBreakerState::HalfOpen,
        }
    }

    /// Checks whether the breaker permits a call. If the breaker is `Open` and the cooldown
    /// has elapsed, transitions to `HalfOpen` and returns `true`. Otherwise returns `false`
    /// when the breaker is currently open.
    #[must_use]
    pub fn allow_call(&self) -> bool {
        let mut guard = self.state.lock();
        match &*guard {
            CircuitState::Closed { .. } | CircuitState::HalfOpen => true,
            CircuitState::Open { opened_at } => {
                if opened_at.elapsed() >= self.config.cooldown {
                    *guard = CircuitState::HalfOpen;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Records a successful call. Closes the breaker if it was half-open and resets the
    /// closed-state failure counter.
    pub fn record_success(&self) {
        let mut guard = self.state.lock();
        *guard = CircuitState::Closed {
            consecutive_failures: 0,
        };
    }

    /// Records a failed call. Increments the closed-state counter; trips the breaker open
    /// if it reaches the configured threshold. A half-open failure re-opens the breaker
    /// immediately.
    pub fn record_failure(&self) {
        let mut guard = self.state.lock();
        match &*guard {
            CircuitState::Closed {
                consecutive_failures,
            } => {
                let next = consecutive_failures.saturating_add(1);
                if next >= self.config.failure_threshold {
                    *guard = CircuitState::Open {
                        opened_at: Instant::now(),
                    };
                } else {
                    *guard = CircuitState::Closed {
                        consecutive_failures: next,
                    };
                }
            }
            CircuitState::HalfOpen | CircuitState::Open { .. } => {
                *guard = CircuitState::Open {
                    opened_at: Instant::now(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_breaker_records_failures_up_to_threshold() {
        let breaker = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 3,
            cooldown: Duration::from_millis(10),
        });
        assert!(breaker.allow_call());
        breaker.record_failure();
        breaker.record_failure();
        assert!(matches!(
            breaker.state(),
            CircuitBreakerState::Closed {
                consecutive_failures: 2
            }
        ));
        breaker.record_failure();
        assert!(matches!(breaker.state(), CircuitBreakerState::Open));
        assert!(!breaker.allow_call(), "open breaker must reject calls");
    }

    #[test]
    fn success_resets_closed_counter() {
        let breaker = CircuitBreaker::new(CircuitBreakerConfig::default());
        breaker.record_failure();
        breaker.record_failure();
        breaker.record_success();
        assert!(matches!(
            breaker.state(),
            CircuitBreakerState::Closed {
                consecutive_failures: 0
            }
        ));
    }

    #[test]
    fn cooldown_transitions_open_to_half_open() {
        let breaker = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(1),
        });
        breaker.record_failure();
        assert!(matches!(breaker.state(), CircuitBreakerState::Open));
        std::thread::sleep(Duration::from_millis(5));
        assert!(breaker.allow_call(), "expired cooldown must allow a probe");
        assert!(matches!(breaker.state(), CircuitBreakerState::HalfOpen));
    }

    #[test]
    fn half_open_failure_reopens() {
        let breaker = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(1),
        });
        breaker.record_failure();
        std::thread::sleep(Duration::from_millis(5));
        assert!(breaker.allow_call());
        breaker.record_failure();
        assert!(matches!(breaker.state(), CircuitBreakerState::Open));
    }

    #[test]
    fn half_open_success_closes() {
        let breaker = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(1),
        });
        breaker.record_failure();
        std::thread::sleep(Duration::from_millis(5));
        assert!(breaker.allow_call());
        breaker.record_success();
        assert!(matches!(
            breaker.state(),
            CircuitBreakerState::Closed {
                consecutive_failures: 0
            }
        ));
    }
}
