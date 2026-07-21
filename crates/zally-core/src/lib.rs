//! Zally domain types.
//!
//! `zally-core` holds the vocabulary every other Zally crate consumes: networks, amounts,
//! heights, transaction identifiers, account identifiers, idempotency keys, and the
//! re-exported ZIP-302 memo type. Every type carries network or unit context where the
//! Public Interfaces spine requires it, and every fallible constructor returns a typed error
//! with a documented retry posture.
//!
//! This crate intentionally has no async-runtime, HTTP, or database dependencies. The only
//! librustzcash dependency is [`zcash_protocol`], which Zally consumes for `Parameters` and
//! `BranchId` plus the regtest `LocalNetwork` carried inside [`Network::Regtest`].

mod account_id;
#[cfg(feature = "serde")]
pub(crate) mod base64_bytes;
mod block_hash;
mod block_height;
mod branch_id;
mod canonicalizer;
mod failure_posture;
mod hash_hex;
mod hold_id;
mod idempotency_key;
mod intent_hash;
mod memo;
mod network;
mod outpoint;
mod payment_recipient;
mod receiver_purpose;
mod signed_payload;
mod transparent_gap;
mod txid;
mod wallet_scan;
mod zatoshis;

pub use account_id::AccountId;
#[cfg(feature = "serde")]
pub(crate) use base64_bytes::is_empty_metadata;
pub use block_hash::BlockHash;
pub use block_height::BlockHeight;
pub use branch_id::BranchId;
pub use canonicalizer::{CanonicalPayment, CanonicalizeError, Canonicalizer};
pub use failure_posture::FailurePosture;
pub use hash_hex::FromRpcHexError;
pub use hold_id::HoldId;
pub use idempotency_key::{IdempotencyKey, IdempotencyKeyError};
pub use intent_hash::{
    DOMAIN_SEPARATOR as INTENT_HASH_DOMAIN_SEPARATOR, IntentHash, IntentHashError, IntentInput,
};
pub use memo::{Memo, MemoBytes, MemoError};
pub use network::{Network, NetworkParameters};
pub use outpoint::OutPoint;
pub use payment_recipient::PaymentRecipient;
pub use receiver_purpose::ReceiverPurpose;
pub use signed_payload::{Amount, AmountUnit, ExpiresAt, SignedPayload, SignedPayloadFormat};
pub use transparent_gap::TransparentGapLimit;
pub use txid::TxId;
pub use wallet_scan::{
    CompactBlockArtifact, CompactChainMetadata, CompactSaplingOutput, CompactSaplingSpend,
    CompactShieldedAction, CompactTransaction, CompactTransparentInput, CompactTransparentOutput,
    TreeStateArtifact,
};
pub use zatoshis::{Zatoshis, ZatoshisError};
