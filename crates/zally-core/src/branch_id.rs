//! ZIP-200 consensus branch identifier.
//!
//! Zally re-exports [`zcash_protocol::consensus::BranchId`] verbatim. The Zally-namespaced
//! path lets error messages and tracing fields reference `zally_core::BranchId` while keeping
//! the upstream type as the single source of truth.

pub use zcash_protocol::consensus::BranchId;
