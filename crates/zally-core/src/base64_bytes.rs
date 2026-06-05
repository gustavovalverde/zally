//! Serde helper that encodes `Vec<u8>` as standard base64 on the wire.
//!
//! Used by [`crate::SignedPayload::bytes`] so the JSON shape matches what
//! the facilitator and the wallet runtime exchange over HTTP. The standard
//! base64 alphabet (`A-Z a-z 0-9 + /`) is chosen over base64url because
//! [`SignedPayload`] travels exclusively in JSON request bodies and never
//! in URL fragments or query strings, and the standard alphabet matches
//! every JSON consumer's default expectation.

#[cfg(feature = "serde")]
use base64::{Engine as _, engine::general_purpose::STANDARD};
#[cfg(feature = "serde")]
use serde::{Deserialize, Deserializer, Serializer};

/// Serializes `bytes` as standard-alphabet base64.
#[cfg(feature = "serde")]
pub(crate) fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&STANDARD.encode(bytes))
}

/// Decodes a standard-alphabet base64 string into `Vec<u8>`.
#[cfg(feature = "serde")]
pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    STANDARD.decode(s).map_err(serde::de::Error::custom)
}

/// Returns `true` when `metadata` is a JSON null or empty object. Used as
/// `skip_serializing_if` so the wire never emits `"metadata": {}` clutter.
#[cfg(feature = "serde")]
#[must_use]
pub(crate) fn is_empty_metadata(metadata: &serde_json::Value) -> bool {
    match metadata {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => map.is_empty(),
        serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_)
        | serde_json::Value::Array(_) => false,
    }
}
