//! Why a proposal or its transaction could not be built.

use zally_core::{BlockHeight, FailurePosture, Zatoshis};

/// Why a proposal or its transaction could not be built.
///
/// Carried by [`crate::StorageError::ProposalRejected`] and by the wallet's
/// `WalletError::ProposalRejected`, so callers can decide what to do next without parsing
/// error text. New kinds may be added as `zcash_client_backend` grows its error vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProposalRejectionKind {
    /// The spendable balance does not cover the payment and its fee.
    ///
    /// The wallet reports this kind as `WalletError::InsufficientBalance`.
    ///
    /// Posture: [`FailurePosture::NotRetryable`] until the balance is replenished or more
    /// notes confirm.
    InsufficientFunds {
        /// Value the proposal needed, including the fee.
        required_zat: Zatoshis,
        /// Spendable value the wallet could draw on.
        available_zat: Zatoshis,
    },
    /// A selected input was locked by a concurrent proposal or PCZT before this proposal
    /// could lock it.
    ///
    /// Posture: [`FailurePosture::Retryable`]; a retry selects around the locked input.
    InputsLocked,
    /// The transaction would exceed the maximum size of a block.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; send a smaller amount or consolidate notes
    /// first.
    TransactionTooLarge,
    /// The caller supplied a target expiry height for a step that is a ZIP 318 canonical
    /// crossing, whose expiry comes from the ZIP 318 rolling window.
    ///
    /// Posture: [`FailurePosture::NotRetryable`] with the same arguments; retry without a
    /// target expiry height.
    ExpiryConflictsWithCanonicalCrossing {
        /// The expiry height the caller asked for.
        requested_expiry_height: BlockHeight,
    },
    /// A recipient's ZIP 316 Revision 2 address has expired, or expires before the
    /// transaction would.
    ///
    /// Posture: [`FailurePosture::NotRetryable`]; ask the recipient for a current address.
    RecipientAddressExpired,
    /// A payment to an Orchard receiver can only be delivered through the Ironwood pool,
    /// which the requested transaction cannot carry.
    ///
    /// Posture: [`FailurePosture::NotRetryable`] with the same arguments.
    OrchardRecipientNeedsIronwood,
    /// The input selector reported a broken contract in the wallet database or change
    /// strategy, such as a selected note it was asked to exclude.
    ///
    /// Posture: [`FailurePosture::RequiresOperator`]; this is a defect, not a balance problem.
    SelectionContractViolated,
    /// Any other rejection; the accompanying reason carries the detail.
    ///
    /// Posture: [`FailurePosture::NotRetryable`].
    Other,
}

impl ProposalRejectionKind {
    /// Operator-facing posture describing what the caller may do.
    #[must_use]
    pub const fn posture(self) -> FailurePosture {
        match self {
            Self::InputsLocked => FailurePosture::Retryable,
            Self::SelectionContractViolated => FailurePosture::RequiresOperator,
            Self::InsufficientFunds { .. }
            | Self::TransactionTooLarge
            | Self::ExpiryConflictsWithCanonicalCrossing { .. }
            | Self::RecipientAddressExpired
            | Self::OrchardRecipientNeedsIronwood
            | Self::Other => FailurePosture::NotRetryable,
        }
    }
}
