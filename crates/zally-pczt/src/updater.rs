//! `Updater` role: mutates PCZT global fields between Creator and Prover.
//!
//! Mirrors the upstream `pczt::roles::updater::Updater` shape but exposes
//! mutation of the `global.expiry_height` field that the upstream `GlobalUpdater`
//! keeps `pub(crate)`. Operates at the postcard wire layer (the same format used
//! by `pczt::Pczt::serialize`) so a Creator-built PCZT can have its expiry
//! committed to a caller-supplied height before Prover and Signer attach
//! signatures.
//!
//! Wire-layer contract: the upstream `pczt::Pczt` is `#[derive(Serialize,
//! Deserialize)]` with field order `global, transparent, sapling, orchard`, and
//! `common::Global` is `#[derive(Serialize, Deserialize)]` with field order
//! `tx_version, version_group_id, consensus_branch_id, fallback_lock_time,
//! expiry_height, coin_type, tx_modifiable, proprietary`. This module pins a
//! mirror of `Global` (`GlobalMirror`) with identical layout, deserialises the
//! first field of the PCZT into it via `postcard::take_from_bytes`, mutates
//! `expiry_height`, reserialises the mirror, and prepends the unchanged
//! remainder of the buffer. The 8-byte `MAGIC_BYTES + PCZT_VERSION` prefix is
//! preserved verbatim.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zally_core::Network;

use crate::bytes::PcztBytes;
use crate::error::PcztError;

/// 4 byte magic + 4 byte little-endian PCZT version prefix.
const PCZT_HEADER_LEN: usize = 8;

/// Mirror of `pczt::common::Global` whose fields are publicly mutable.
///
/// Field order, types, and serde derives are wire-compatible with the upstream
/// `Global` struct: postcard is positional, so an identical layout round-trips
/// through `postcard::take_from_bytes` and `postcard::to_allocvec` without
/// altering any unrelated bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct GlobalMirror {
    tx_version: u32,
    version_group_id: u32,
    consensus_branch_id: u32,
    fallback_lock_time: Option<u32>,
    expiry_height: u32,
    coin_type: u32,
    tx_modifiable: u8,
    proprietary: BTreeMap<String, Vec<u8>>,
}

/// Mutates PCZT global fields between Creator and Prover.
///
/// The Updater is the sole zally role with permission to commit caller-supplied
/// global metadata into a PCZT before proving and signing. `with_global_expiry_height`
/// is the only mutation currently exposed because it is the only one zally's
/// callers need; subsequent slices that need to mutate `coin_type`,
/// `fallback_lock_time`, or `tx_modifiable` extend this surface.
#[derive(Debug)]
pub struct Updater {
    bytes: PcztBytes,
    pending_expiry_height: Option<u32>,
}

impl Updater {
    /// Constructs an Updater that will mutate `pczt`.
    #[must_use]
    pub fn new(pczt: PcztBytes) -> Self {
        Self {
            bytes: pczt,
            pending_expiry_height: None,
        }
    }

    /// Returns the network this Updater is bound to.
    #[must_use]
    pub fn network(&self) -> Network {
        self.bytes.network()
    }

    /// Commits `height` as the PCZT's `global.expiry_height` when [`Updater::finish`]
    /// runs.
    ///
    /// Mirrors the upstream `pczt::roles::updater::Updater::update_global_with` shape
    /// but reaches the `pub(crate)` `expiry_height` field via a wire-format mirror.
    #[must_use]
    pub fn with_global_expiry_height(mut self, height: u32) -> Self {
        self.pending_expiry_height = Some(height);
        self
    }

    /// Applies any pending mutations and returns the updated PCZT bytes.
    ///
    /// `not_retryable` on a malformed PCZT (`PcztError::ParseFailed`) or a postcard
    /// reserialisation failure (`PcztError::SerializeFailed`).
    pub fn finish(self) -> Result<PcztBytes, PcztError> {
        let Some(target_height) = self.pending_expiry_height else {
            return Ok(self.bytes);
        };

        let network = self.bytes.network();
        let raw = self.bytes.into_bytes();
        if raw.len() < PCZT_HEADER_LEN {
            return Err(PcztError::ParseFailed {
                reason: "PCZT shorter than the 8-byte magic+version header".to_string(),
            });
        }
        let (header, payload) = raw.split_at(PCZT_HEADER_LEN);

        let (mut global, tail) =
            postcard::take_from_bytes::<GlobalMirror>(payload).map_err(|err| {
                PcztError::ParseFailed {
                    reason: format!("PCZT global section decode failed: {err}"),
                }
            })?;
        global.expiry_height = target_height;

        let mut rebuilt = Vec::with_capacity(raw.len());
        rebuilt.extend_from_slice(header);
        rebuilt = postcard::to_extend(&global, rebuilt).map_err(|err| {
            PcztError::SerializeFailed {
                reason: format!("PCZT global section encode failed: {err}"),
            }
        })?;
        rebuilt.extend_from_slice(tail);

        Ok(PcztBytes::from_serialized(rebuilt, network))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal serialised `pczt::Pczt` whose `global.expiry_height` is `expiry`.
    ///
    /// Exercises the same `pczt::Pczt::serialize` codepath the Creator role calls so the
    /// mirror layout is validated against the upstream encoding, not against the mirror
    /// itself.
    fn build_minimal_pczt(expiry: u32) -> Vec<u8> {
        let pczt_struct: pczt::Pczt = postcard::from_bytes(&pczt_with_expiry(expiry))
            .expect("minimal Pczt deserialises through upstream parser");
        pczt_struct.serialize()
    }

    /// Hand-built minimal postcard payload for a `pczt::Pczt`.
    ///
    /// Used by `build_minimal_pczt` to obtain a real upstream `pczt::Pczt`, which is then
    /// re-serialised to produce a byte sequence the upstream parser accepts.
    fn pczt_with_expiry(expiry: u32) -> Vec<u8> {
        use serde::Serialize;
        #[derive(Serialize)]
        struct EmptyTransparent {
            inputs: Vec<()>,
            outputs: Vec<()>,
        }
        #[derive(Serialize)]
        struct EmptyShielded {
            spends: Vec<()>,
            outputs: Vec<()>,
            value_sum: (i64, i64),
            anchor: [u8; 32],
            bsk: Option<[u8; 32]>,
        }
        #[derive(Serialize)]
        struct OrchardShielded {
            actions: Vec<()>,
            flags: u8,
            value_sum: (i64, i64),
            anchor: [u8; 32],
            zkproof: Option<Vec<u8>>,
            bsk: Option<[u8; 32]>,
        }
        #[derive(Serialize)]
        struct PcztMirror {
            global: GlobalMirror,
            transparent: EmptyTransparent,
            sapling: EmptyShielded,
            orchard: OrchardShielded,
        }
        let mirror = PcztMirror {
            global: GlobalMirror {
                tx_version: 5,
                version_group_id: 0x26A7_270A,
                consensus_branch_id: 0xC2D6_D0B4,
                fallback_lock_time: None,
                expiry_height: expiry,
                coin_type: 1,
                tx_modifiable: 0,
                proprietary: BTreeMap::new(),
            },
            transparent: EmptyTransparent {
                inputs: vec![],
                outputs: vec![],
            },
            sapling: EmptyShielded {
                spends: vec![],
                outputs: vec![],
                value_sum: (0, 0),
                anchor: [0_u8; 32],
                bsk: None,
            },
            orchard: OrchardShielded {
                actions: vec![],
                flags: 0,
                value_sum: (0, 0),
                anchor: [0_u8; 32],
                zkproof: None,
                bsk: None,
            },
        };
        postcard::to_allocvec(&mirror).expect("minimal Pczt mirror serialises")
    }

    #[test]
    fn finish_without_mutation_round_trips_bytes() {
        let original = PcztBytes::from_serialized(build_minimal_pczt(100), Network::regtest());
        let unchanged = Updater::new(original.clone())
            .finish()
            .expect("finish round-trip");
        assert_eq!(unchanged.as_bytes(), original.as_bytes());
        assert_eq!(unchanged.network(), Network::regtest());
    }

    #[test]
    fn with_global_expiry_height_mutates_only_expiry() {
        let original = PcztBytes::from_serialized(build_minimal_pczt(100), Network::regtest());
        let mutated = Updater::new(original)
            .with_global_expiry_height(4_321_098)
            .finish()
            .expect("finish applied mutation");

        let parsed = pczt::Pczt::parse(mutated.as_bytes())
            .expect("upstream Pczt parses mutated bytes");
        assert_eq!(*parsed.global().expiry_height(), 4_321_098);
        assert_eq!(*parsed.global().tx_version(), 5);
        assert_eq!(*parsed.global().consensus_branch_id(), 0xC2D6_D0B4);
    }

    #[test]
    fn finish_rejects_truncated_header() {
        let short = PcztBytes::from_serialized(vec![0_u8; 4], Network::regtest());
        let err = Updater::new(short)
            .with_global_expiry_height(1)
            .finish()
            .expect_err("truncated header must error");
        assert!(matches!(err, PcztError::ParseFailed { .. }));
    }
}
