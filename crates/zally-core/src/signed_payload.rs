//! Chain-neutral envelope around a signed payment transaction.
//!
//! Produced by the wallet runtime (`zspend-runtime` in zpay's workspace) on the
//! `/v1/payments/sign` response and consumed by the facilitator (`zpay-runtime`)
//! on the `/x402/v2/settle` body. The envelope keeps the on-chain transaction
//! bytes opaque to the wire vocabulary so integrators porting to non-Zcash
//! chains rewrite adapters, not the surface this type defines.
//!
//! See Proposal-0003 D-3 in zpay's `docs/proposals/0003-agent-wallet-production-architecture.md`
//! for the architectural rationale; D-10 (CAIP-typed identifiers + decimal-string
//! amounts) for the field-level decisions; and D-15 (brand names in package names
//! only) for the absence of any zentity/zpay/zally tag in the wire schema.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Chain-neutral envelope around a signed payment transaction.
///
/// The Zcash adapter sets [`Self::format`] to [`SignedPayloadFormat::PcztV1`]
/// and writes a ZIP-48 v1 PCZT into [`Self::bytes`]. Future formats (raw Solana
/// transaction, raw EVM transaction, etc.) plug in by adding variants to
/// [`SignedPayloadFormat`] without touching the surrounding shape.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SignedPayload {
    /// Identifies the wire format of [`Self::bytes`]. The facilitator dispatches
    /// extract-and-broadcast on this discriminant.
    pub format: SignedPayloadFormat,

    /// Signed transaction bytes in the format identified by [`Self::format`].
    /// On the JSON wire this serializes as base64; in Rust it is the raw byte
    /// slice the format's encoder produced.
    #[cfg_attr(feature = "serde", serde(with = "crate::base64_bytes"))]
    pub bytes: Vec<u8>,

    /// Chain-specific transaction identifier as a printable string. Zcash:
    /// lowercase hex of the ZIP-244 txid. Solana: base58 of the transaction
    /// signature. Stellar: lowercase hex of the operation hash. EVM: lowercase
    /// hex of the keccak-256 transaction hash with the `0x` prefix elided.
    pub tx_id: String,

    /// Fee paid to the chain for inclusion. Chain-neutral on purpose: the
    /// currency identifies the chain unit, the value is a decimal string, and
    /// the unit names whether the value is in base or display units.
    pub fee: Amount,

    /// The chain-specific point in time after which the chain will refuse to
    /// include the transaction.
    pub expires_at: ExpiresAt,

    /// Format- or chain-specific overflow. Object keys are scoped by integrator
    /// namespace, e.g. `"zentity.final"` for the PCZT extractor-ready marker
    /// or `"zcash.consensus_branch_id"` for the network's NU activation. An
    /// empty object on the wire is omitted via `skip_serializing_if`.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "crate::is_empty_metadata")
    )]
    pub metadata: serde_json::Value,
}

/// Wire format of [`SignedPayload::bytes`].
///
/// `#[non_exhaustive]` so new formats land as additive changes. The Zcash v1
/// adapter only constructs and accepts [`Self::PcztV1`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
#[non_exhaustive]
pub enum SignedPayloadFormat {
    /// Partially Created Zcash Transaction, ZIP-48 v1, extractor-ready.
    PcztV1,
}

/// Chain-neutral amount: a decimal-string `value` in `currency`'s `unit`.
///
/// Replaces `_zat`-typed fields on the wire (D-10). The fauzec faucet and the
/// Zcash adapter convert in and out of [`crate::Zatoshis`] at the type
/// boundary; the wire never carries chain-specific integer aliases.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Amount {
    /// ISO-4217-style identifier of the currency. Zcash: `"ZEC"`. USD-Coin on
    /// any chain: `"USDC"`. Solana native: `"SOL"`.
    pub currency: String,

    /// Decimal string representation of the amount. No leading sign; no
    /// thousands separators. Examples: `"0.5"`, `"50000000"`. Decimal string
    /// is chosen over float to defeat IEEE-754 representation drift across
    /// JSON parsers.
    pub value: String,

    /// Whether [`Self::value`] is denominated in the chain's smallest base
    /// unit or in the display unit. Most wire surfaces use [`AmountUnit::Base`]
    /// to avoid rounding loss; display amounts are surfaced to users only.
    pub unit: AmountUnit,
}

/// Unit of an [`Amount`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
#[non_exhaustive]
pub enum AmountUnit {
    /// Smallest indivisible chain unit. Zcash: zatoshi. Solana: lamport. EVM:
    /// wei. Stellar: stroop.
    Base,
    /// Human-display unit. Zcash: ZEC. Solana: SOL. EVM: ETH. Stellar: XLM.
    Display,
}

/// Chain-specific expiry, tagged by the chain's time abstraction.
///
/// Wire shape: `{ "kind": "block_height", "value": 4047100 }` for Zcash;
/// `{ "kind": "slot", "value": 123456789 }` for Solana; etc. The tagged
/// representation lets a multi-chain consumer dispatch on `kind` without
/// trying to parse a freeform string.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(tag = "kind", content = "value", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ExpiresAt {
    /// Zcash consensus expiry: chain block height past which the network
    /// refuses to mine the transaction.
    BlockHeight(u32),
    /// Solana slot expiry.
    Slot(u64),
    /// EVM block-number expiry.
    BlockNumber(u64),
    /// Unix-timestamp expiry in seconds since the epoch.
    TimestampSeconds(u64),
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pczt_v1_round_trips_through_json() -> Result<(), serde_json::Error> {
        let payload = SignedPayload {
            format: SignedPayloadFormat::PcztV1,
            bytes: vec![0xde, 0xad, 0xbe, 0xef],
            tx_id: "abcd1234".to_owned(),
            fee: Amount {
                currency: "ZEC".to_owned(),
                value: "1000".to_owned(),
                unit: AmountUnit::Base,
            },
            expires_at: ExpiresAt::BlockHeight(4_047_100),
            metadata: json!({ "zentity.final": true }),
        };

        let wire = serde_json::to_value(&payload)?;
        let back: SignedPayload = serde_json::from_value(wire.clone())?;
        assert_eq!(payload, back);

        // bytes serialize as base64
        assert_eq!(wire["bytes"].as_str(), Some("3q2+7w=="));
        // format is kebab-case
        assert_eq!(wire["format"].as_str(), Some("pczt-v1"));
        // expires_at is tagged
        assert_eq!(wire["expires_at"]["kind"].as_str(), Some("block_height"));
        assert_eq!(wire["expires_at"]["value"].as_u64(), Some(4_047_100));
        Ok(())
    }

    #[test]
    fn empty_metadata_is_omitted_on_the_wire() -> Result<(), serde_json::Error> {
        let payload = SignedPayload {
            format: SignedPayloadFormat::PcztV1,
            bytes: Vec::new(),
            tx_id: String::new(),
            fee: Amount {
                currency: "ZEC".to_owned(),
                value: "0".to_owned(),
                unit: AmountUnit::Base,
            },
            expires_at: ExpiresAt::BlockHeight(0),
            metadata: json!({}),
        };

        let wire = serde_json::to_value(&payload)?;
        assert!(
            wire.get("metadata").is_none(),
            "metadata: {{}} must be omitted: {wire:?}"
        );
        Ok(())
    }
}
