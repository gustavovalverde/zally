//! Zally operator-facing wallet API.

mod account_balance;
mod capabilities;
mod circuit_breaker;
mod event;
mod exposed_address;
mod metrics;
mod pczt;
mod pending_transparent_inputs;
mod received_note;
mod retry;
mod spend;
mod status;
mod sync;
mod unspent_note;
mod wallet;
mod wallet_error;
mod wallet_options;

pub use account_balance::AccountBalance;
pub use capabilities::{Capability, SealingCapability, StorageCapability, WalletCapabilities};
pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerState};
pub use event::{WalletEvent, WalletEventStream};
pub use exposed_address::ExposedAddress;
pub use metrics::WalletMetrics;
pub use pending_transparent_inputs::{PendingTransparentInput, PendingTransparentInputs};
pub use received_note::ShieldedReceiveRecord;
pub use retry::{HasFailurePosture, RetryPolicy, with_retry};
pub use spend::{
    ParsedPayment, PaymentRequest, Proposal, ProposalPlan, SendOutcome, SendPaymentPlan,
    ShieldTransparentPlan,
};
pub use status::{SyncStatus, WalletStatus};
pub use sync::{
    SyncDriver, SyncDriverOptions, SyncDriverStatus, SyncErrorSnapshot, SyncHandle, SyncOutcome,
    SyncSnapshot, SyncSnapshotStream,
};
pub use unspent_note::UnspentShieldedNote;
pub use wallet::{Wallet, WalletBuilder};
pub use wallet_error::WalletError;
pub use wallet_options::WalletOptions;
