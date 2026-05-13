//! Age-encrypted file sealing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use age::x25519::{Identity, Recipient};
use age::{Decryptor, Encryptor};
use async_trait::async_trait;
use secrecy::ExposeSecret;

use crate::sealing::{SealingError, SeedSealing};
use crate::seed_material::SeedMaterial;

const IDENTITY_SIDECAR_SUFFIX: &str = ".age-identity";

/// Options for [`AgeFileSealing`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AgeFileSealingOptions {
    /// Filesystem path at which the encrypted seed is stored.
    ///
    /// The age identity is stored at the same path with the `.age-identity` suffix appended.
    pub seed_path: PathBuf,
}

impl AgeFileSealingOptions {
    /// Constructs options for the given seed-file path.
    #[must_use]
    pub fn at_path(seed_path: PathBuf) -> Self {
        Self { seed_path }
    }
}

/// Age-encrypted file [`SeedSealing`] implementation.
///
/// On first [`seal_seed`](SeedSealing::seal_seed), a fresh age X25519 identity is generated and
/// stored in a sidecar file `<seed_path>.age-identity`. The seed itself is encrypted with the
/// identity's public recipient and written to `<seed_path>`. Both files are written atomically
/// via write-to-temp-then-rename.
pub struct AgeFileSealing {
    options: AgeFileSealingOptions,
}

impl AgeFileSealing {
    /// Constructs a new sealing backed by the given options.
    #[must_use]
    pub fn new(options: AgeFileSealingOptions) -> Self {
        Self { options }
    }

    fn identity_path(&self) -> PathBuf {
        let mut s = self.options.seed_path.clone().into_os_string();
        s.push(IDENTITY_SIDECAR_SUFFIX);
        s.into()
    }
}

#[async_trait]
impl SeedSealing for AgeFileSealing {
    async fn seal_seed(&self, seed: &SeedMaterial) -> Result<(), SealingError> {
        let seed_path = self.options.seed_path.clone();
        let identity_path = self.identity_path();
        let bytes_owned: Vec<u8> = seed.expose_secret().to_vec();

        tokio::task::spawn_blocking(move || seal_blocking(&seed_path, &identity_path, &bytes_owned))
            .await
            .map_err(|join_err| SealingError::BlockingTaskFailed {
                reason: join_err.to_string(),
            })?
    }

    async fn unseal_seed(&self) -> Result<SeedMaterial, SealingError> {
        let seed_path = self.options.seed_path.clone();
        let identity_path = self.identity_path();

        tokio::task::spawn_blocking(move || unseal_blocking(&seed_path, &identity_path))
            .await
            .map_err(|join_err| SealingError::BlockingTaskFailed {
                reason: join_err.to_string(),
            })?
    }
}

fn seal_blocking(
    seed_path: &Path,
    identity_path: &Path,
    seed_bytes: &[u8],
) -> Result<(), SealingError> {
    let identity = load_or_create_identity(identity_path)?;
    let recipient = identity.to_public();
    write_encrypted_seed(seed_path, &recipient, seed_bytes)
}

fn unseal_blocking(seed_path: &Path, identity_path: &Path) -> Result<SeedMaterial, SealingError> {
    if !seed_path
        .try_exists()
        .map_err(|e| SealingError::ReadFailed {
            reason: e.to_string(),
        })?
    {
        return Err(SealingError::NoSealedSeed);
    }
    let identity = load_identity(identity_path)?;
    let ciphertext = std::fs::read(seed_path).map_err(|e| SealingError::ReadFailed {
        reason: e.to_string(),
    })?;
    let decryptor =
        Decryptor::new(ciphertext.as_slice()).map_err(|e| SealingError::DecryptionFailed {
            reason: e.to_string(),
        })?;
    let recipients_decryptor = match decryptor {
        Decryptor::Recipients(rd) => rd,
        Decryptor::Passphrase(_) => {
            return Err(SealingError::DecryptionFailed {
                reason: "expected recipient-encrypted age file, found passphrase".into(),
            });
        }
    };
    let mut reader = recipients_decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| SealingError::DecryptionFailed {
            reason: e.to_string(),
        })?;
    let mut plaintext = Vec::with_capacity(64);
    reader
        .read_to_end(&mut plaintext)
        .map_err(|e| SealingError::DecryptionFailed {
            reason: e.to_string(),
        })?;
    SeedMaterial::from_raw_bytes(plaintext).map_err(SealingError::from)
}

fn load_or_create_identity(identity_path: &Path) -> Result<Identity, SealingError> {
    let exists = identity_path
        .try_exists()
        .map_err(|e| SealingError::ReadFailed {
            reason: e.to_string(),
        })?;
    if exists {
        load_identity(identity_path)
    } else {
        let identity = Identity::generate();
        let encoded = identity.to_string();
        write_atomic(identity_path, encoded.expose_secret().as_bytes())?;
        Ok(identity)
    }
}

fn load_identity(identity_path: &Path) -> Result<Identity, SealingError> {
    let encoded =
        std::fs::read_to_string(identity_path).map_err(|e| SealingError::AgeIdentityFailed {
            reason: format!("could not read identity file: {e}"),
        })?;
    encoded
        .trim()
        .parse::<Identity>()
        .map_err(|err| SealingError::AgeIdentityFailed {
            reason: err.to_string(),
        })
}

fn write_encrypted_seed(
    seed_path: &Path,
    recipient: &Recipient,
    seed_bytes: &[u8],
) -> Result<(), SealingError> {
    let encryptor =
        Encryptor::with_recipients(vec![Box::new(recipient.clone())]).ok_or_else(|| {
            SealingError::EncryptionFailed {
                reason: "age recipient set is empty".into(),
            }
        })?;
    let mut ciphertext = Vec::new();
    let mut writer =
        encryptor
            .wrap_output(&mut ciphertext)
            .map_err(|e| SealingError::EncryptionFailed {
                reason: e.to_string(),
            })?;
    writer
        .write_all(seed_bytes)
        .map_err(|e| SealingError::EncryptionFailed {
            reason: e.to_string(),
        })?;
    writer
        .finish()
        .map_err(|e| SealingError::EncryptionFailed {
            reason: e.to_string(),
        })?;
    write_atomic(seed_path, &ciphertext)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SealingError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mnemonic::Mnemonic;

    #[tokio::test]
    async fn age_file_round_trip() -> Result<(), SealingError> {
        let dir = tempfile::tempdir().map_err(|e| SealingError::WriteFailed {
            reason: e.to_string(),
        })?;
        let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(
            dir.path().join("wallet.age"),
        ));

        let mnemonic = Mnemonic::generate();
        let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
        let original = seed.expose_secret().to_vec();

        sealing.seal_seed(&seed).await?;
        let unsealed = sealing.unseal_seed().await?;
        assert_eq!(unsealed.expose_secret(), original.as_slice());
        Ok(())
    }

    #[tokio::test]
    async fn age_file_unseal_without_seal_returns_no_sealed_seed() -> Result<(), SealingError> {
        let dir = tempfile::tempdir().map_err(|e| SealingError::ReadFailed {
            reason: e.to_string(),
        })?;
        let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(
            dir.path().join("wallet.age"),
        ));
        let outcome = sealing.unseal_seed().await;
        assert!(matches!(outcome, Err(SealingError::NoSealedSeed)));
        Ok(())
    }
}
