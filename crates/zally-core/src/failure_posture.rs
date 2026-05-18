//! Operator-facing classification of every Zally boundary failure.

/// Operator-facing classification of a boundary failure.
///
/// Three classes are sufficient for wallet-side lifecycle decisions: transient backend
/// trouble that benefits from retry, conditions that require operator action before the
/// request can succeed, and caller bugs that retrying will not help. The three labels are
/// the canonical operator-facing names used in Zally's error vocabulary, metrics, and
/// readiness payloads.
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
            Self::RequiresOperator => "requires_operator",
            Self::NotRetryable => "not_retryable",
        }
    }

    /// `true` when the caller may issue the same request again.
    ///
    /// Equivalent to `matches!(self, Self::Retryable)`. Provided for callers that just need
    /// a boolean retry decision.
    #[must_use]
    pub const fn allows_retry(self) -> bool {
        matches!(self, Self::Retryable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_stable() {
        assert_eq!(FailurePosture::Retryable.label(), "retryable");
        assert_eq!(
            FailurePosture::RequiresOperator.label(),
            "requires_operator"
        );
        assert_eq!(FailurePosture::NotRetryable.label(), "not_retryable");
    }

    #[test]
    fn allows_retry_only_for_retryable() {
        assert!(FailurePosture::Retryable.allows_retry());
        assert!(!FailurePosture::RequiresOperator.allows_retry());
        assert!(!FailurePosture::NotRetryable.allows_retry());
    }
}
