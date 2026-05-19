//! Zally PCZT roles.
//!
//! Provides the [`Creator`], [`Prover`], [`Signer`], [`Combiner`], and [`Extractor`] role
//! wrappers around the upstream `pczt` crate, plus the [`PcztBytes`] transport type.
//! Network cross-role validation runs at every role boundary.

mod bytes;
mod combiner;
mod creator;
mod error;
mod extractor;
mod prover;
mod signer;

pub use bytes::PcztBytes;
pub use combiner::Combiner;
pub use creator::Creator;
pub use error::PcztError;
pub use extractor::{ExtractedTransaction, Extractor};
pub use prover::Prover;
pub use signer::Signer;
