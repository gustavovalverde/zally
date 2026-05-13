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
mod block_height;
mod branch_id;
mod idempotency_key;
mod memo;
mod network;
mod payment_recipient;
mod receiver_purpose;
mod txid;
mod zatoshis;

pub use account_id::AccountId;
pub use block_height::BlockHeight;
pub use branch_id::BranchId;
pub use idempotency_key::{IdempotencyKey, IdempotencyKeyError};
pub use memo::{Memo, MemoBytes, MemoError};
pub use network::{Network, NetworkParameters};
pub use payment_recipient::PaymentRecipient;
pub use receiver_purpose::ReceiverPurpose;
pub use txid::TxId;
pub use zatoshis::{Zatoshis, ZatoshisError};
