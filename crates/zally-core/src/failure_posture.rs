//! Operator-facing classification of every Zally boundary failure.

/// Operator-facing classification of a boundary failure.
///
/// Four classes are sufficient for wallet-side lifecycle decisions: transient backend
/// trouble that benefits from retry, an expired source boundary whose bounded operation must
/// restart against a fresh one, conditions that require operator action before the request
/// can succeed, and caller bugs that retrying will not help. The four labels are the
/// canonical operator-facing names used in Zally's error vocabulary, metrics, and readiness
/// payloads.
///
/// Every Zally boundary error (`StorageError`, `ChainSourceError`, `SubmitterError`,
/// `SealingError`, `KeyDerivationError`, `PcztError`, `WalletError`) carries this posture
/// directly on each variant or exposes a `posture()` method that maps variants onto it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum FailurePosture {
    /// Transient backend trouble. Retry with backoff is the appropriate response; the
    /// circuit breaker trips on consecutive failures.
    Retryable,
    /// The source boundary the caller pinned (a chain epoch, an event cursor) expired while
    /// the caller's bounded operation was still running. Re-issuing the same pinned request
    /// can never succeed; the caller acquires a fresh boundary and restarts the operation
    /// from it. The source answered precisely and is serving, so this is not evidence of
    /// backend trouble.
    Restartable,
    /// An operator must intervene before the request can succeed (capability missing,
    /// configuration mismatch, upstream returning malformed bytes). Callers must surface
    /// this and stop retrying.
    RequiresOperator,
    /// The request itself is wrong or out of bounds. Callers fix the request and re-issue;
    /// retrying the same input fails again.
    NotRetryable,
}

impl FailurePosture {
    /// Stable kebab-case label for metrics, logs, and readiness payloads.
    ///
    /// Do not rename without coordinating dashboards: the label is the operator-facing
    /// identifier.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Retryable => "retryable",
            Self::Restartable => "restartable",
            Self::RequiresOperator => "requires_operator",
            Self::NotRetryable => "not_retryable",
        }
    }

    /// `true` when the caller may issue the operation again.
    ///
    /// [`Self::Restartable`] qualifies only once the caller has re-acquired its source
    /// boundary; repeating the identical pinned request fails again.
    #[must_use]
    pub const fn allows_retry(self) -> bool {
        matches!(self, Self::Retryable | Self::Restartable)
    }

    /// `true` when the failure is evidence that the boundary itself is unhealthy.
    ///
    /// The wallet circuit breaker counts only these. Every other posture leaves the breaker
    /// exactly as it found it: none of them is evidence of health either, so none may clear a
    /// failure streak or close a half-open probe.
    #[must_use]
    pub const fn trips_circuit_breaker(self) -> bool {
        matches!(self, Self::Retryable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_stable() {
        assert_eq!(FailurePosture::Retryable.label(), "retryable");
        assert_eq!(FailurePosture::Restartable.label(), "restartable");
        assert_eq!(
            FailurePosture::RequiresOperator.label(),
            "requires_operator"
        );
        assert_eq!(FailurePosture::NotRetryable.label(), "not_retryable");
    }

    #[test]
    fn allows_retry_covers_retryable_and_restartable() {
        assert!(FailurePosture::Retryable.allows_retry());
        assert!(FailurePosture::Restartable.allows_retry());
        assert!(!FailurePosture::RequiresOperator.allows_retry());
        assert!(!FailurePosture::NotRetryable.allows_retry());
    }

    #[test]
    fn only_retryable_trips_the_circuit_breaker() {
        assert!(FailurePosture::Retryable.trips_circuit_breaker());
        assert!(!FailurePosture::Restartable.trips_circuit_breaker());
        assert!(!FailurePosture::RequiresOperator.trips_circuit_breaker());
        assert!(!FailurePosture::NotRetryable.trips_circuit_breaker());
    }
}
