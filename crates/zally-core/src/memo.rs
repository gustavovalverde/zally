//! ZIP-302 memo wrapper.
//!
//! Re-exports the canonical types from [`zcash_protocol::memo`]. Re-implementing the enum
//! field-by-field would duplicate the ZIP-302 encoding rules; the upstream type is the single
//! source of truth and is the right shape for Zally's public surface.

pub use zcash_protocol::memo::{Error as MemoError, Memo, MemoBytes};
