//! Zally PCZT roles.
//!
//! Slice 4 ships the [`Creator`], [`Signer`], [`Combiner`], and [`Extractor`] role
//! wrappers around the upstream `pczt` crate, plus the [`PcztBytes`] transport type. The
//! deep proposal-building path is wired to live storage in Slice 5; the role surfaces and
//! the network cross-role validation are stable across that addition.
//!
//! See RFC-0004 for design rationale.

mod combiner;
mod creator;
mod extractor;
mod pczt_bytes;
mod pczt_error;
mod signer;

pub use combiner::Combiner;
pub use creator::Creator;
pub use extractor::{ExtractedTransaction, Extractor};
pub use pczt_bytes::PcztBytes;
pub use pczt_error::PcztError;
pub use signer::Signer;
