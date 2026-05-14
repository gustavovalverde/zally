//! Zally PCZT roles.
//!
//! Provides the [`Creator`], [`Signer`], [`Combiner`], and [`Extractor`] role wrappers
//! around the upstream `pczt` crate, plus the [`PcztBytes`] transport type. Network
//! cross-role validation runs at every role boundary.

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
