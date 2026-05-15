//! Zally operator-facing wallet API.

mod capabilities;
mod circuit_breaker;
mod event;
mod metrics;
mod pczt;
mod received_note;
mod retry;
mod spend;
mod status;
mod sync;
mod unspent_note;
mod wallet;
mod wallet_error;

pub use capabilities::{Capability, SealingCapability, StorageCapability, WalletCapabilities};
pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerState};
pub use event::{WalletEvent, WalletEventStream};
pub use metrics::WalletMetrics;
pub use received_note::ShieldedReceiveRecord;
pub use retry::{IsRetryable, RetryPolicy, with_retry};
pub use spend::{
    FeeStrategy, ParsedPayment, PaymentRequest, Proposal, ProposalPlan, SendOutcome,
    SendPaymentPlan, ShieldTransparentPlan,
};
pub use status::{SyncStatus, WalletStatus};
pub use sync::{
    SyncDriver, SyncDriverOptions, SyncDriverStatus, SyncErrorSnapshot, SyncHandle, SyncOutcome,
    SyncSnapshot, SyncSnapshotStream,
};
pub use unspent_note::UnspentShieldedNote;
pub use wallet::Wallet;
pub use wallet_error::WalletError;
