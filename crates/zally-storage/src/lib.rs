//! Zally wallet storage trait and `SQLite` implementation.

mod account_balance_row;
mod error;
mod exposed_address_row;
mod hold_row;
mod pending_broadcast_input_row;
mod proposal_rejection_kind;
mod reverted_broadcast_row;
mod send_step_row;
mod sqlite;
mod wallet;
mod zip218;

pub use account_balance_row::AccountBalanceRow;
pub use error::StorageError;
pub use exposed_address_row::ExposedAddressRow;
pub use hold_row::{HeldNote, HoldRow};
pub use pending_broadcast_input_row::PendingBroadcastInputRow;
pub use proposal_rejection_kind::{ProposalRejectionKind, ShieldedBlockLimit};
pub use reverted_broadcast_row::RevertedBroadcastRow;
pub use send_step_row::SendStepRow;
pub use sqlite::{Sqlite, SqliteOptions};
pub use wallet::{
    ChainTips, CommitmentTreeRoots, HoldRecord, PendingBroadcastRecord,
    PendingBroadcastTransaction, PreparedTransaction, ProposalPaymentRequest, ProposalSummary,
    ReceivedShieldedNoteRow, RevertedBlocks, ScanRequest, ScanResult, ShieldTransparentRequest,
    StorageKind, TransparentReceiverRow, TransparentUtxoRow, UnmineableHold,
    UnspentShieldedNoteRow, WalletStorage,
};
