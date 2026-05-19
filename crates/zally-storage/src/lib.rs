//! Zally wallet storage trait and `SQLite` implementation.

mod account_balance_row;
mod error;
mod exposed_address_row;
mod filtered_wallet_db;
mod pending_broadcast_input_row;
mod sqlite;
mod wallet;

pub use account_balance_row::AccountBalanceRow;
pub use error::StorageError;
pub use exposed_address_row::ExposedAddressRow;
pub use pending_broadcast_input_row::PendingBroadcastInputRow;
pub use sqlite::{Sqlite, SqliteOptions};
pub use wallet::{
    PendingBroadcastRecord, PreparedTransaction, ProposalPaymentRequest, ProposalSummary,
    ReceivedShieldedNoteRow, ScanRequest, ScanResult, ShieldTransparentRequest, StorageKind,
    TransparentReceiverRow, TransparentUtxoRow, UnspentShieldedNoteRow, WalletStorage,
};
