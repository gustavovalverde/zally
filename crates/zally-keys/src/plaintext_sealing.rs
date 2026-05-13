//! Plaintext seed storage. NEVER USE IN PRODUCTION.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::sealing::{SealingError, SeedSealing};
use crate::seed_material::SeedMaterial;

/// Plaintext seed storage. NEVER USE IN PRODUCTION.
///
/// Stores the raw seed bytes at the configured path with no encryption. Available only behind
/// the `unsafe_plaintext_seed` cargo feature. The wallet layer emits a `WARN`-level tracing
/// event on every `Wallet::open` and `Wallet::create` when this sealing is in use; the impl
/// itself stays silent (one warning per wallet lifetime, not one per cryptographic operation).
pub struct PlaintextSealing {
    seed_path: PathBuf,
}

impl PlaintextSealing {
    /// Constructs a new plaintext sealing at the given path.
    #[must_use]
    pub fn new(seed_path: PathBuf) -> Self {
        Self { seed_path }
    }
}

#[async_trait]
impl SeedSealing for PlaintextSealing {
    async fn seal_seed(&self, seed: &SeedMaterial) -> Result<(), SealingError> {
        let path = self.seed_path.clone();
        let bytes = seed.expose_secret().to_vec();
        tokio::task::spawn_blocking(move || seal_blocking(&path, &bytes))
            .await
            .map_err(|e| SealingError::BlockingTaskFailed {
                reason: e.to_string(),
            })?
    }

    async fn unseal_seed(&self) -> Result<SeedMaterial, SealingError> {
        let path = self.seed_path.clone();
        tokio::task::spawn_blocking(move || unseal_blocking(&path))
            .await
            .map_err(|e| SealingError::BlockingTaskFailed {
                reason: e.to_string(),
            })?
    }
}

fn seal_blocking(path: &Path, bytes: &[u8]) -> Result<(), SealingError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SealingError::WriteFailed {
            reason: format!("could not create directory {}: {e}", parent.display()),
        })?;
    }
    let mut tmp_path = path.to_path_buf().into_os_string();
    tmp_path.push(".tmp");
    let tmp_path: PathBuf = tmp_path.into();
    std::fs::write(&tmp_path, bytes).map_err(|e| SealingError::WriteFailed {
        reason: e.to_string(),
    })?;
    std::fs::rename(&tmp_path, path).map_err(|e| SealingError::WriteFailed {
        reason: e.to_string(),
    })
}

fn unseal_blocking(path: &Path) -> Result<SeedMaterial, SealingError> {
    if !path.try_exists().map_err(|e| SealingError::ReadFailed {
        reason: e.to_string(),
    })? {
        return Err(SealingError::NoSealedSeed);
    }
    let bytes = std::fs::read(path).map_err(|e| SealingError::ReadFailed {
        reason: e.to_string(),
    })?;
    SeedMaterial::from_raw_bytes(bytes).map_err(SealingError::from)
}
