//! Zally operator-facing wallet API.

mod capabilities;
mod circuit_breaker;
mod event;
mod metrics;
mod pczt;
mod retry;
mod spend;
mod sync;
mod wallet;
mod wallet_error;

pub use capabilities::{Capability, SealingCapability, StorageCapability, WalletCapabilities};
pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerState};
pub use event::{WalletEvent, WalletEventStream};
pub use metrics::WalletMetrics;
pub use retry::{IsRetryable, RetryPolicy, with_retry};
pub use spend::{
    FeeStrategy, ParsedPayment, PaymentRequest, Proposal, ProposalPlan, SendOutcome,
    SendPaymentPlan,
};
pub use sync::SyncOutcome;
pub use wallet::Wallet;
pub use wallet_error::WalletError;
