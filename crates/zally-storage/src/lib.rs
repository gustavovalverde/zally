//! Zally wallet storage trait and `SQLite` implementation.

mod account_balance_row;
mod exposed_address_row;
mod pending_broadcast_input_row;
mod sqlite;
mod storage_error;
mod wallet_storage;

pub use account_balance_row::AccountBalanceRow;
pub use exposed_address_row::ExposedAddressRow;
pub use pending_broadcast_input_row::PendingBroadcastInputRow;
pub use sqlite::{SqliteWalletStorage, SqliteWalletStorageOptions};
pub use storage_error::StorageError;
pub use wallet_storage::{
    PendingBroadcastRecord, PreparedTransaction, ProposalPaymentRequest, ProposalSummary,
    ReceivedShieldedNoteRow, ScanRequest, ScanResult, ShieldTransparentRequest,
    TransparentReceiverRow, TransparentUtxoRow, UnspentShieldedNoteRow, WalletStorage,
};
