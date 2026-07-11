//! Payment-disclosure export bound to a network-tagged [`Wallet`](crate::Wallet).

use std::fmt;

use zally_core::{Network, PaymentRecipient, TxId, Zatoshis};
use zally_pczt::{
    PaymentDisclosureExportError, PaymentDisclosureExportPlan as PcztDisclosureExportPlan,
    PcztBytes,
};
use zcash_payment_disclosure::PaymentDisclosureProfile;

use crate::{Wallet, WalletError};

/// Inputs to [`Wallet::export_payment_disclosure`].
#[derive(Clone, Debug)]
pub struct ExportPaymentDisclosurePlan {
    /// Transaction whose payment is disclosed.
    pub transaction_id: TxId,
    /// External recipient selected from the retained finalized PCZT.
    pub recipient: PaymentRecipient,
    /// Exact payment amount selected from the retained finalized PCZT.
    pub amount_zat: Zatoshis,
    /// Caller-supplied message or interactive challenge.
    pub message: Vec<u8>,
    /// Immutable disclosure profile to produce.
    pub profile: PaymentDisclosureProfile,
}

impl ExportPaymentDisclosurePlan {
    /// Constructs a payment-disclosure export plan.
    #[must_use]
    pub fn new(
        transaction_id: TxId,
        recipient: PaymentRecipient,
        amount_zat: Zatoshis,
        message: Vec<u8>,
        profile: PaymentDisclosureProfile,
    ) -> Self {
        Self {
            transaction_id,
            recipient,
            amount_zat,
            message,
            profile,
        }
    }
}

/// Portable payment disclosure plus the network on which it is valid.
#[derive(Clone, Eq, PartialEq)]
pub struct PaymentDisclosure {
    network: Network,
    portable: zcash_payment_disclosure::PaymentDisclosure,
}

impl fmt::Debug for PaymentDisclosure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaymentDisclosure")
            .field("network", &self.network)
            .field("profile", &self.portable.profile())
            .field("transaction_id", &self.portable.transaction_id())
            .finish_non_exhaustive()
    }
}

impl PaymentDisclosure {
    /// Returns the network on which this disclosure is valid.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// Returns the portable disclosure for encoding or verification.
    #[must_use]
    pub const fn portable(&self) -> &zcash_payment_disclosure::PaymentDisclosure {
        &self.portable
    }

    /// Encodes the portable disclosure in its canonical profile format.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.portable.to_bytes()
    }
}

impl Wallet {
    /// Exports a payment disclosure from finalized PCZT material retained at extraction.
    ///
    /// The sealed seed is unsealed only after the source PCZT is found and the recipient network
    /// matches this wallet. Disclosure bytes and retained PCZT contents are never logged.
    ///
    /// `not_retryable` when source material is absent, the selected payment is unsupported or
    /// ambiguous, or retained proof inputs are invalid. `requires_operator` on missing proving
    /// parameters, storage integrity failures, or network mismatch. `retryable` only when the
    /// underlying storage or sealing boundary reports a transient failure.
    pub async fn export_payment_disclosure(
        &self,
        plan: ExportPaymentDisclosurePlan,
    ) -> Result<PaymentDisclosure, WalletError> {
        if plan.recipient.network() != self.network() {
            return Err(WalletError::NetworkMismatch {
                storage: self.network(),
                requested: plan.recipient.network(),
            });
        }
        let params = self.network().to_parameters();
        let decoded_recipient =
            zcash_keys::address::Address::decode(&params, plan.recipient.encoded());
        let has_profile_receiver = match (plan.profile, decoded_recipient) {
            (PaymentDisclosureProfile::Zip311Draft1, Some(address)) => {
                address.to_sapling_address().is_some()
            }
            (
                PaymentDisclosureProfile::ZallyIronwood,
                Some(zcash_keys::address::Address::Unified(unified)),
            ) => unified.orchard().is_some(),
            _ => false,
        };
        if !has_profile_receiver {
            return Err(PaymentDisclosureExportError::RecipientUnsupported.into());
        }
        let finalized_pczt_bytes = self
            .inner
            .storage
            .find_finalized_pczt_bytes(plan.transaction_id)
            .await?
            .ok_or(WalletError::PaymentDisclosureSourceMissing {
                transaction_id: plan.transaction_id,
            })?;
        let seed = self.inner.sealing.unseal_seed().await?;
        let finalized_pczt = PcztBytes::from_serialized(finalized_pczt_bytes, self.network());
        let portable = zally_pczt::export_payment_disclosure(
            &finalized_pczt,
            PcztDisclosureExportPlan::new(
                plan.transaction_id,
                plan.recipient,
                plan.amount_zat,
                plan.message,
                plan.profile,
            ),
            &seed,
        )?;
        Ok(PaymentDisclosure {
            network: self.network(),
            portable,
        })
    }
}
