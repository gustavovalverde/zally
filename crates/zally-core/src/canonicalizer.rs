//! Per-scheme canonicalization of `payment_request` wire values.
//!
//! The wallet runtime (`zspend-runtime` in zpay's workspace) receives the
//! `payment_request` as `{ scheme, value }` on the wire (Proposal-0003 D-11)
//! and recomputes [`crate::IntentHash`] over the parsed tuple to match the
//! RAR-bound hash from the issuer's mint. Each payment scheme has its own
//! grammar (ZIP-321, Solana Pay, SEP-0007, EIP-681); each scheme ships its
//! own [`Canonicalizer`] implementation. The trait keeps the parse boundary
//! consistent so a multi-scheme wallet dispatches on `scheme` without
//! diverging behavior per scheme.
//!
//! This module defines only the trait and the canonical output shape. The
//! ZIP-321 implementation lives next to [`crate::PaymentRecipient`] in
//! `zally-wallet`; non-Zcash schemes ship their own crates implementing the
//! trait.

use crate::signed_payload::AmountUnit;

/// Canonical, scheme-neutral payment tuple produced by a [`Canonicalizer`].
///
/// These fields feed [`crate::IntentInput`] directly: the canonicalizer's job
/// is to make sure two wire representations of the same payment intent always
/// produce the same canonical tuple, so the wallet's [`crate::IntentHash`]
/// recomputation matches the issuer's bound hash byte-for-byte.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalPayment {
    /// CAIP-10 account identifier of the recipient.
    /// Format: `{namespace}:{reference}:{address}`. Example for Zcash testnet:
    /// `"zcash:test:utest1qq..."`.
    pub recipient_caip10: String,

    /// Amount value in the unit identified by [`Self::amount_unit`]. Base unit
    /// is the canonical wire choice; [`AmountUnit::Display`] is only used for
    /// user-facing surfaces.
    pub amount_value: u64,

    /// Unit interpretation of [`Self::amount_value`].
    pub amount_unit: AmountUnit,
}

/// Errors raised by [`Canonicalizer::canonicalize`].
///
/// Variants are intentionally coarse: the wallet runtime maps each to a
/// fixed wire error code (`payment_request_invalid`) without leaking parser
/// internals to the agent caller.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CanonicalizeError {
    /// The `scheme` discriminant did not match the canonicalizer asked to
    /// handle it. Surfaced when a registry dispatch misroutes a value to the
    /// wrong implementation.
    #[error("scheme mismatch: canonicalizer for {expected:?} received {actual:?}")]
    SchemeMismatch {
        /// The scheme this canonicalizer handles.
        expected: &'static str,
        /// The scheme tag on the value.
        actual: String,
    },

    /// The `value` did not parse as a well-formed expression of the scheme.
    #[error("payment_request value invalid: {reason}")]
    Invalid {
        /// Operator-facing reason; never includes user-supplied bytes verbatim.
        reason: String,
    },

    /// The value carried more than one payment entry. v1 wallet runtimes
    /// accept exactly one entry per `payment_request`; batch flows are a
    /// post-v1 additive feature.
    #[error("payment_request must carry exactly one payment in v1")]
    MultiplePayments,

    /// The value parsed but the amount was missing or zero. Recurring across
    /// schemes; surfaced as a single variant so the wallet does not branch on
    /// scheme-specific error shapes.
    #[error("payment_request amount missing or zero")]
    AmountMissingOrZero,
}

/// Parses + canonicalizes a `payment_request.value` for a specific
/// scheme.
///
/// One canonicalizer per scheme; the [`Canonicalizer::scheme`] return value is
/// the discriminant a dispatch table keys on. Implementations MUST:
///
/// 1. Reject values whose embedded scheme disagrees with [`Self::scheme`].
/// 2. Reject batch payments with [`CanonicalizeError::MultiplePayments`].
/// 3. Reject missing or zero amounts with [`CanonicalizeError::AmountMissingOrZero`].
/// 4. Emit recipient as CAIP-10 in lowercase namespace form.
pub trait Canonicalizer: Send + Sync {
    /// Scheme discriminant matching the `scheme` tag on `payment_request`.
    /// Lowercase, kebab-case where the scheme uses a multi-word name.
    /// Examples: `"zip321"`, `"solana-pay"`, `"sep-0007"`, `"eip-681"`.
    fn scheme(&self) -> &'static str;

    /// Parses `value` and returns the canonical tuple. See trait docs for the
    /// invariants every implementation must enforce.
    fn canonicalize(&self, value: &str) -> Result<CanonicalPayment, CanonicalizeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubZip321;

    impl Canonicalizer for StubZip321 {
        fn scheme(&self) -> &'static str {
            "zip321"
        }

        fn canonicalize(&self, value: &str) -> Result<CanonicalPayment, CanonicalizeError> {
            if !value.starts_with("zcash:") {
                return Err(CanonicalizeError::Invalid {
                    reason: "missing zcash: prefix".to_owned(),
                });
            }
            Ok(CanonicalPayment {
                recipient_caip10: "zcash:test:utest1qq...".to_owned(),
                amount_value: 50_000_000,
                amount_unit: AmountUnit::Base,
            })
        }
    }

    #[test]
    fn trait_object_dispatches_on_scheme() -> Result<(), CanonicalizeError> {
        let canon: Box<dyn Canonicalizer> = Box::new(StubZip321);
        assert_eq!(canon.scheme(), "zip321");
        let out = canon.canonicalize("zcash:utest1qq...")?;
        assert_eq!(out.amount_value, 50_000_000);
        Ok(())
    }

    #[test]
    fn rejects_invalid_value() {
        let canon = StubZip321;
        let result = canon.canonicalize("not-a-zip321");
        assert!(matches!(result, Err(CanonicalizeError::Invalid { .. })));
    }
}
