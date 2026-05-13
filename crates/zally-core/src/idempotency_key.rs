//! Caller-supplied idempotency key for send operations.

/// Caller-supplied idempotency key for send operations.
///
/// Must be 1-128 ASCII printable characters (0x20-0x7E inclusive). Validated at construction;
/// the [`Display`](std::fmt::Display) form of every [`IdempotencyKeyError`] names the valid
/// range so operators can fix offending input from the message alone.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Returns the key as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for IdempotencyKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for IdempotencyKey {
    type Error = IdempotencyKeyError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        validate(s)?;
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for IdempotencyKey {
    type Error = IdempotencyKeyError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        validate(&s)?;
        Ok(Self(s))
    }
}

fn validate(s: &str) -> Result<(), IdempotencyKeyError> {
    let byte_count = s.len();
    if !(1..=128).contains(&byte_count) {
        return Err(IdempotencyKeyError::InvalidLength { byte_count });
    }
    for (byte_offset, byte) in s.bytes().enumerate() {
        if !(0x20..=0x7E).contains(&byte) {
            return Err(IdempotencyKeyError::InvalidCharacter { byte_offset });
        }
    }
    Ok(())
}

/// Error returned when [`IdempotencyKey`] construction fails.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum IdempotencyKeyError {
    /// The key length is outside the valid range.
    ///
    /// `not_retryable`: the caller must shorten or pad the input.
    #[error("idempotency key length is {byte_count}; valid range is 1-128 characters")]
    InvalidLength {
        /// Length of the rejected input, in bytes.
        byte_count: usize,
    },

    /// The key contains a byte outside the printable ASCII range.
    ///
    /// `not_retryable`: the caller must replace the offending byte.
    #[error(
        "idempotency key has a non-printable character at byte offset {byte_offset}; valid range is 0x20-0x7E"
    )]
    InvalidCharacter {
        /// Byte offset of the first invalid character in the rejected input.
        byte_offset: usize,
    },
}

impl IdempotencyKeyError {
    /// Whether the same input may succeed on retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::InvalidLength { .. } | Self::InvalidCharacter { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_valid_ascii() -> Result<(), IdempotencyKeyError> {
        let key = IdempotencyKey::try_from("invoice-2026-05-12-abc")?;
        assert_eq!(key.as_str(), "invoice-2026-05-12-abc");
        Ok(())
    }

    #[test]
    fn idempotency_key_empty_rejected() {
        let outcome = IdempotencyKey::try_from("");
        assert!(matches!(
            outcome,
            Err(IdempotencyKeyError::InvalidLength { byte_count: 0 })
        ));
        if let Err(e) = outcome {
            assert!(!e.is_retryable());
        }
    }

    #[test]
    fn idempotency_key_too_long_rejected() {
        let s = "a".repeat(129);
        let outcome = IdempotencyKey::try_from(s.as_str());
        assert!(matches!(
            outcome,
            Err(IdempotencyKeyError::InvalidLength { byte_count: 129 })
        ));
    }

    #[test]
    fn idempotency_key_non_ascii_rejected() {
        let outcome = IdempotencyKey::try_from("café");
        assert!(matches!(
            outcome,
            Err(IdempotencyKeyError::InvalidCharacter { .. })
        ));
    }

    #[test]
    fn idempotency_key_control_char_rejected() {
        let outcome = IdempotencyKey::try_from("abc\ndef");
        assert!(matches!(
            outcome,
            Err(IdempotencyKeyError::InvalidCharacter { byte_offset: 3 })
        ));
    }
}
