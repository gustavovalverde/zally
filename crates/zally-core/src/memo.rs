//! ZIP-302 memo wrapper.
//!
//! Re-exports the canonical types from [`zcash_protocol::memo`]. The upstream type preserves
//! ZIP-302 encoding semantics and avoids a parallel Zally-owned memo enum.

pub use zcash_protocol::memo::{Error as MemoError, Memo, MemoBytes};
