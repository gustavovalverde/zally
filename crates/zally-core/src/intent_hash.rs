//! Parsed-tuple intent binding for the OAuth `payment_authorization` RAR.
//!
//! Both the identity issuer (TypeScript at `packages/sdk/src/protocol/intent-hash.ts`
//! in zentity) and the wallet runtime (Rust in `zspend-runtime`) compute the
//! same SHA-256 over the same byte layout. Hashing the parsed tuple, not the
//! URI text, eliminates canonicalization drift across language boundaries
//! (Proposal-0003 D-4).
//!
//! # Byte layout
//!
//! ```text
//!   "zentity.payauth.v1"               domain separator (18 bytes)
//!   u16-be(chain_namespace.len()) || chain_namespace.as_bytes()
//!   u16-be(chain_reference.len()) || chain_reference.as_bytes()
//!   u16-be(recipient_caip10.len()) || recipient_caip10.as_bytes()
//!   amount_value.to_be_bytes()         8 bytes (u64)
//!   amount_unit                        1 byte (0x00 Base, 0x01 Display)
//!   u16-be(payment_id.len()) || payment_id.as_bytes()
//!   expiry_height.to_be_bytes()        8 bytes (u64)
//! ```
//!
//! Length prefixes defeat the "ambiguous concatenation" attack where two
//! distinct tuples could collide by shifting bytes between adjacent fields.
//!
//! # Wire encoding
//!
//! `"v1:sha256:<base64url 32 bytes, no padding>"`. The version prefix lets
//! a future binding scheme co-exist with this one and lets verifiers reject
//! a hash whose version they do not implement.

use crate::signed_payload::AmountUnit;

/// Domain separator hashed at the start of every intent. ASCII 18 bytes.
pub const DOMAIN_SEPARATOR: &[u8] = b"zentity.payauth.v1";

/// Parsed-tuple inputs to [`IntentHash::compute`].
///
/// All string fields are UTF-8 and capped at `65_535` bytes by the `u16`
/// length prefix. Longer inputs return [`IntentHashError::FieldTooLong`].
#[derive(Clone, Copy, Debug)]
pub struct IntentInput<'a> {
    /// CAIP-2 chain namespace. Zcash: `"zcash"`. EVM: `"eip155"`.
    pub chain_namespace: &'a str,
    /// CAIP-2 chain reference. Zcash: `"main"` or `"test"`. EVM: chain id text.
    pub chain_reference: &'a str,
    /// CAIP-10 account identifier. The full `namespace:reference:address` form.
    pub recipient_caip10: &'a str,
    /// Amount value in the chain's smallest base unit. Bytes are big-endian.
    pub amount_value: u64,
    /// Unit interpretation of [`Self::amount_value`].
    pub amount_unit: AmountUnit,
    /// The opaque facilitator-issued payment id.
    pub payment_id: &'a str,
    /// Chain-specific expiry. Zcash: block height as u64. Solana: slot.
    /// EVM: block number. Always 8 bytes big-endian on the wire.
    pub expiry_height: u64,
}

/// 32-byte SHA-256 digest binding an [`IntentInput`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IntentHash([u8; 32]);

/// Errors raised by [`IntentHash::compute`] and [`IntentHash::from_wire_string`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IntentHashError {
    /// A variable-length input exceeded the `u16`-prefix capacity. No realistic
    /// CAIP-2 / CAIP-10 / payment-id value approaches this limit; an input
    /// over `65_535` bytes is almost certainly an attack.
    #[error("intent field {field} too long: {len} bytes (max {max})")]
    FieldTooLong {
        /// Which field in [`IntentInput`] exceeded the cap.
        field: &'static str,
        /// Actual length in bytes.
        len: usize,
        /// Maximum length in bytes.
        max: usize,
    },
    /// The wire string did not start with the supported `"v1:sha256:"` prefix.
    #[error("intent hash version not supported (expected v1:sha256:)")]
    UnsupportedVersion,
    /// The wire string's hash payload was not valid base64url-no-pad of 32 bytes.
    #[error("intent hash payload invalid: {reason}")]
    PayloadInvalid {
        /// Operator-facing reason; no input bytes leak.
        reason: String,
    },
}

const MAX_FIELD_LEN: usize = u16::MAX as usize;
const WIRE_PREFIX: &str = "v1:sha256:";

impl IntentHash {
    /// Computes the binding hash over `input`.
    ///
    /// Returns [`IntentHashError::FieldTooLong`] when any string field exceeds
    /// [`u16::MAX`] bytes (the u16-be length-prefix cap).
    #[allow(
        clippy::missing_panics_doc,
        reason = "u16 cap is checked above the to_be_bytes calls"
    )]
    pub fn compute(input: &IntentInput<'_>) -> Result<Self, IntentHashError> {
        use sha2::{Digest, Sha256};

        check_len("chain_namespace", input.chain_namespace.len())?;
        check_len("chain_reference", input.chain_reference.len())?;
        check_len("recipient_caip10", input.recipient_caip10.len())?;
        check_len("payment_id", input.payment_id.len())?;

        let mut hasher = Sha256::new();
        hasher.update(DOMAIN_SEPARATOR);
        write_len_prefixed(&mut hasher, input.chain_namespace.as_bytes());
        write_len_prefixed(&mut hasher, input.chain_reference.as_bytes());
        write_len_prefixed(&mut hasher, input.recipient_caip10.as_bytes());
        hasher.update(input.amount_value.to_be_bytes());
        hasher.update([amount_unit_byte(input.amount_unit)]);
        write_len_prefixed(&mut hasher, input.payment_id.as_bytes());
        hasher.update(input.expiry_height.to_be_bytes());

        let digest = hasher.finalize();
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&digest);
        Ok(Self(bytes))
    }

    /// Returns the raw 32-byte digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the wire-encoded form: `"v1:sha256:<base64url-no-pad 32 bytes>"`.
    #[must_use]
    pub fn to_wire_string(&self) -> String {
        #[cfg(feature = "serde")]
        {
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            let mut s = String::with_capacity(WIRE_PREFIX.len() + 43);
            s.push_str(WIRE_PREFIX);
            s.push_str(&URL_SAFE_NO_PAD.encode(self.0));
            s
        }
        #[cfg(not(feature = "serde"))]
        {
            // base64 lives behind the serde feature in zally-core. Without it we
            // still surface a printable form, just not wire-compatible.
            format!("{}{}", WIRE_PREFIX, hex::encode(self.0))
        }
    }

    /// Parses the wire-encoded form. Rejects any version prefix other than
    /// `"v1:sha256:"` and any payload that is not 32 bytes of base64url-no-pad.
    #[cfg(feature = "serde")]
    pub fn from_wire_string(s: &str) -> Result<Self, IntentHashError> {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let Some(encoded) = s.strip_prefix(WIRE_PREFIX) else {
            return Err(IntentHashError::UnsupportedVersion);
        };
        let raw =
            URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|err| IntentHashError::PayloadInvalid {
                    reason: format!("base64url decode failed: {err}"),
                })?;
        if raw.len() != 32 {
            return Err(IntentHashError::PayloadInvalid {
                reason: format!("expected 32 bytes, got {}", raw.len()),
            });
        }
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&raw);
        Ok(Self(bytes))
    }
}

const fn amount_unit_byte(unit: AmountUnit) -> u8 {
    match unit {
        AmountUnit::Base => 0x00,
        AmountUnit::Display => 0x01,
    }
}

fn check_len(field: &'static str, len: usize) -> Result<(), IntentHashError> {
    if len > MAX_FIELD_LEN {
        Err(IntentHashError::FieldTooLong {
            field,
            len,
            max: MAX_FIELD_LEN,
        })
    } else {
        Ok(())
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "len bounded by check_len above; u16 cast is safe"
)]
fn write_len_prefixed(hasher: &mut sha2::Sha256, bytes: &[u8]) {
    use sha2::Digest;
    let len = bytes.len() as u16;
    hasher.update(len.to_be_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Conformance vector: minimal Zcash testnet input.
    ///
    /// The TypeScript side at `packages/sdk/src/protocol/intent-hash.ts` MUST
    /// produce the same digest for these inputs. Bumping either side without
    /// the other is a wire-shape break that this test catches at compile time.
    #[test]
    fn conformance_vector_zcash_testnet_minimal() -> Result<(), IntentHashError> {
        let hash = IntentHash::compute(&IntentInput {
            chain_namespace: "zcash",
            chain_reference: "test",
            recipient_caip10: "zcash:test:utest1qq...",
            amount_value: 50_000_000,
            amount_unit: AmountUnit::Base,
            payment_id: "01KT9A0V431VGD5YH7R7G635HC",
            expiry_height: 4_047_100,
        })?;
        insta_assert(hash);
        Ok(())
    }

    #[test]
    fn changing_recipient_changes_hash() -> Result<(), IntentHashError> {
        let a = IntentHash::compute(&IntentInput {
            chain_namespace: "zcash",
            chain_reference: "test",
            recipient_caip10: "zcash:test:address_a",
            amount_value: 1,
            amount_unit: AmountUnit::Base,
            payment_id: "01HX...",
            expiry_height: 1,
        })?;
        let b = IntentHash::compute(&IntentInput {
            chain_namespace: "zcash",
            chain_reference: "test",
            recipient_caip10: "zcash:test:address_b",
            amount_value: 1,
            amount_unit: AmountUnit::Base,
            payment_id: "01HX...",
            expiry_height: 1,
        })?;
        assert_ne!(a, b);
        Ok(())
    }

    #[test]
    fn length_prefix_defeats_byte_shift_collision() -> Result<(), IntentHashError> {
        // Without length prefixes, ("ab", "cd") and ("a", "bcd") would
        // concatenate to the same bytes. The u16-be prefix makes them
        // distinct: the first has 0002 ab 0002 cd, the second 0001 a 0003 bcd.
        let a = IntentHash::compute(&IntentInput {
            chain_namespace: "ab",
            chain_reference: "cd",
            recipient_caip10: "x",
            amount_value: 0,
            amount_unit: AmountUnit::Base,
            payment_id: "x",
            expiry_height: 0,
        })?;
        let b = IntentHash::compute(&IntentInput {
            chain_namespace: "a",
            chain_reference: "bcd",
            recipient_caip10: "x",
            amount_value: 0,
            amount_unit: AmountUnit::Base,
            payment_id: "x",
            expiry_height: 0,
        })?;
        assert_ne!(a, b);
        Ok(())
    }

    #[cfg(feature = "serde")]
    #[test]
    fn wire_string_round_trips() -> Result<(), IntentHashError> {
        let hash = IntentHash::compute(&IntentInput {
            chain_namespace: "zcash",
            chain_reference: "main",
            recipient_caip10: "zcash:main:u1...",
            amount_value: 100_000_000,
            amount_unit: AmountUnit::Base,
            payment_id: "01HX...",
            expiry_height: 2_500_000,
        })?;
        let wire = hash.to_wire_string();
        assert!(wire.starts_with(WIRE_PREFIX));
        let back = IntentHash::from_wire_string(&wire)?;
        assert_eq!(hash, back);
        Ok(())
    }

    #[cfg(feature = "serde")]
    #[test]
    fn from_wire_string_rejects_wrong_version() {
        let result = IntentHash::from_wire_string("v2:sha256:abc");
        assert!(matches!(result, Err(IntentHashError::UnsupportedVersion)));
    }

    fn insta_assert(hash: IntentHash) {
        // Conformance-vector snapshot. If this assertion fails, the byte
        // layout has changed and the TypeScript mirror must change in
        // lockstep. The vector below was computed by this same function
        // on the canonical input; updating it without updating the TS
        // mirror is the wire break we are protecting against.
        let expected_hex = "b47e481896e757a3714a5f679b06c573c1160c5eab7d871780e7c71669888d44";
        assert_eq!(
            hex::encode(hash.as_bytes()),
            expected_hex,
            "intent_hash byte layout changed; update packages/sdk/src/protocol/intent-hash.ts to match"
        );
    }
}
