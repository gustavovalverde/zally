//! PCZT-role error vocabulary.

use zally_core::Network;

/// Error returned by [`crate::Creator`], [`crate::Prover`], [`crate::Signer`],
/// [`crate::Combiner`], or [`crate::Extractor`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PcztError {
    /// PCZT bytes could not be decoded.
    ///
    /// `not_retryable`: a malformed PCZT fails the same way every time.
    #[error("PCZT parse failed: {reason}")]
    ParseFailed {
        /// Underlying decoder error.
        reason: String,
    },

    /// PCZT could not be re-serialised.
    ///
    /// `not_retryable`.
    #[error("PCZT serialization failed: {reason}")]
    SerializeFailed {
        /// Underlying encoder error.
        reason: String,
    },

    /// Network mismatch between the PCZT and the configured caller.
    ///
    /// `requires_operator`: configuration error.
    #[error("PCZT network mismatch: pczt={pczt_network:?}, configured={configured_network:?}")]
    NetworkMismatch {
        /// Network embedded in the PCZT.
        pczt_network: Network,
        /// Network the caller is configured for.
        configured_network: Network,
    },

    /// No spend in the PCZT can be signed by keys derivable from the supplied seed.
    ///
    /// `not_retryable`: the operator must supply the seed that controls these inputs.
    #[error("no keys derivable from the supplied seed match any of the PCZT's spends")]
    NoMatchingKeys,

    /// PCZT is not yet ready for the requested role.
    ///
    /// `requires_operator`: another role (signer, combiner) must run first.
    #[error("PCZT is not in the required state for this role: {reason}")]
    NotFinalized {
        /// Description of which finalisation step is missing.
        reason: String,
    },

    /// Two PCZTs disagree on inputs or outputs and cannot be combined.
    ///
    /// `not_retryable`: caller must reconcile the conflicting PCZTs upstream.
    #[error("PCZTs cannot be combined: {reason}")]
    CombineConflict {
        /// Description of the conflict.
        reason: String,
    },

    /// Posture varies per upstream cause.
    #[error("upstream PCZT error: {reason}")]
    UpstreamFailed {
        /// Underlying error description.
        reason: String,
        /// Whether retrying the same call may succeed.
        is_retryable: bool,
    },

    /// Sapling prover/verifying parameters are not available on disk.
    ///
    /// `requires_operator`: download the Sapling spend/output parameters into the
    /// platform-default `ZcashParams` directory.
    #[error(
        "Sapling parameters are not available; install sapling-spend.params and \
         sapling-output.params into the platform-default location"
    )]
    ProverUnavailable,
}

impl PcztError {
    /// Whether the same call may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::UpstreamFailed { is_retryable, .. } => *is_retryable,
            Self::ParseFailed { .. }
            | Self::SerializeFailed { .. }
            | Self::NetworkMismatch { .. }
            | Self::NoMatchingKeys
            | Self::NotFinalized { .. }
            | Self::CombineConflict { .. }
            | Self::ProverUnavailable => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pczt_error_retryable_match_complete() {
        let variants = [
            PcztError::ParseFailed { reason: "x".into() },
            PcztError::SerializeFailed { reason: "x".into() },
            PcztError::NetworkMismatch {
                pczt_network: Network::Mainnet,
                configured_network: Network::Testnet,
            },
            PcztError::NoMatchingKeys,
            PcztError::NotFinalized { reason: "x".into() },
            PcztError::CombineConflict { reason: "x".into() },
            PcztError::UpstreamFailed {
                reason: "x".into(),
                is_retryable: false,
            },
            PcztError::ProverUnavailable,
        ];
        for e in variants {
            let _ = e.is_retryable();
        }
    }
}
