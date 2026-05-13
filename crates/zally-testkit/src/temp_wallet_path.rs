//! Temporary directory fixture for wallet path scaffolding.

use std::io;
use std::path::PathBuf;

use tempfile::TempDir;

/// Owns a temporary directory and exposes wallet-database and sealed-seed paths inside it.
///
/// The directory is removed when `TempWalletPath` is dropped.
pub struct TempWalletPath {
    dir: TempDir,
}

impl TempWalletPath {
    /// Creates a new temporary wallet directory.
    pub fn create() -> Result<Self, io::Error> {
        Ok(Self {
            dir: tempfile::tempdir()?,
        })
    }

    /// Path to the wallet database file (`wallet.db` inside the directory).
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.dir.path().join("wallet.db")
    }

    /// Path to the sealed seed file (`wallet.age` inside the directory).
    #[must_use]
    pub fn seed_path(&self) -> PathBuf {
        self.dir.path().join("wallet.age")
    }

    /// The temporary directory itself.
    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        self.dir.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_wallet_path_cleanup() -> Result<(), io::Error> {
        let saved: PathBuf;
        {
            let temp = TempWalletPath::create()?;
            saved = temp.dir().to_path_buf();
            assert!(saved.exists());
        }
        assert!(!saved.exists());
        Ok(())
    }
}
