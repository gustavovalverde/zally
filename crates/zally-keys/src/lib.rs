//! Zally seed lifecycle.

mod age_file_sealing;
mod mnemonic;
#[cfg(feature = "unsafe_plaintext_seed")]
mod plaintext_sealing;
mod sealing;
mod seed_material;
mod ufvk;

pub use age_file_sealing::{AgeFileSealing, AgeFileSealingOptions};
pub use mnemonic::{Mnemonic, MnemonicError};
#[cfg(feature = "unsafe_plaintext_seed")]
pub use plaintext_sealing::PlaintextSealing;
pub use sealing::{SealingError, SeedSealing};
pub use seed_material::{SeedMaterial, SeedMaterialError};
pub use ufvk::{KeyDerivationError, derive_ufvk};
