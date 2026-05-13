//! In-memory `SeedSealing` fixture.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zally_keys::{SealingError, SeedMaterial, SeedSealing};

type Slot = Arc<Mutex<Option<Vec<u8>>>>;

/// In-memory [`SeedSealing`] implementation for tests.
///
/// Holds the unsealed seed bytes inside a [`parking_lot::Mutex`]. Two `InMemorySealing`
/// handles produced by [`InMemorySealing::new`] and [`InMemorySealing::shared_with`] share
/// the same backing slot, which simulates "open with a fresh handle to the same sealed seed"
/// for `Wallet::open` round-trip tests without touching the filesystem.
pub struct InMemorySealing {
    slot: Slot,
}

impl InMemorySealing {
    /// New sealing with its own backing slot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: Arc::new(Mutex::new(None)),
        }
    }

    /// New sealing that shares its backing slot with `self`.
    ///
    /// Use to simulate a wallet re-open against an in-memory sealing without touching the
    /// filesystem.
    #[must_use]
    pub fn shared_with(&self) -> Self {
        Self {
            slot: Arc::clone(&self.slot),
        }
    }
}

impl Default for InMemorySealing {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SeedSealing for InMemorySealing {
    async fn seal_seed(&self, seed: &SeedMaterial) -> Result<(), SealingError> {
        let mut guard = self.slot.lock();
        *guard = Some(seed.expose_secret().to_vec());
        drop(guard);
        Ok(())
    }

    async fn unseal_seed(&self) -> Result<SeedMaterial, SealingError> {
        let cloned = {
            let guard = self.slot.lock();
            guard.clone()
        };
        let bytes = cloned.ok_or(SealingError::NoSealedSeed)?;
        SeedMaterial::from_raw_bytes(bytes).map_err(SealingError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zally_keys::Mnemonic;

    #[tokio::test]
    async fn in_memory_sealing_round_trip() -> Result<(), SealingError> {
        let sealing = InMemorySealing::new();
        let mnemonic = Mnemonic::generate();
        let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
        let original = seed.expose_secret().to_vec();

        sealing.seal_seed(&seed).await?;
        let unsealed = sealing.unseal_seed().await?;
        assert_eq!(unsealed.expose_secret(), original.as_slice());
        Ok(())
    }

    #[tokio::test]
    async fn in_memory_sealing_shared_state() -> Result<(), SealingError> {
        let primary = InMemorySealing::new();
        let mnemonic = Mnemonic::generate();
        let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
        primary.seal_seed(&seed).await?;

        let secondary = primary.shared_with();
        let unsealed = secondary.unseal_seed().await?;
        assert_eq!(unsealed.expose_secret(), seed.expose_secret());
        Ok(())
    }

    #[tokio::test]
    async fn in_memory_sealing_unseal_without_seal_returns_no_sealed_seed() {
        let sealing = InMemorySealing::new();
        let outcome = sealing.unseal_seed().await;
        assert!(matches!(outcome, Err(SealingError::NoSealedSeed)));
    }
}
