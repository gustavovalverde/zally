//! Zally wallet storage trait and `SQLite` implementation.

mod sqlite;
mod storage_error;
mod wallet_storage;

pub use sqlite::{SqliteWalletStorage, SqliteWalletStorageOptions};
pub use storage_error::StorageError;
pub use wallet_storage::{
    PreparedTransaction, ProposalPaymentRequest, ProposalSummary, ScanRequest, ScanResult,
    UnspentShieldedNoteRow, WalletStorage,
};
