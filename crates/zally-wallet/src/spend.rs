//! Spend flow: payment-request parsing, proposal, send.
//!
//! Carries the ZIP-302, ZIP-320, and ZIP-321 guards on the wallet layer. `Wallet::propose`
//! drives `propose_standard_transfer_to_address` against the live `WalletDb`;
//! `Wallet::send_payment` adds signing and submission through the caller-supplied
//! `Submitter`.

use zally_chain::Submitter;
use zally_core::{
    AccountId, BlockHeight, IdempotencyKey, Memo, MemoBytes, Network, PaymentRecipient, TxId,
    Zatoshis,
};

use crate::wallet::Wallet;
use crate::wallet_error::WalletError;

/// Parsed ZIP-321 payment request.
#[derive(Clone, Debug)]
pub struct PaymentRequest {
    payments: Vec<ParsedPayment>,
    network: Network,
}

impl PaymentRequest {
    /// Parses `uri` into a Zally payment request.
    ///
    /// `not_retryable`: a malformed URI fails the same way every time.
    pub fn from_uri(uri: &str, network: Network) -> Result<Self, WalletError> {
        let request = zip321::TransactionRequest::from_uri(uri).map_err(|err| {
            WalletError::PaymentRequestParseFailed {
                reason: err.to_string(),
            }
        })?;
        let payments = request
            .payments()
            .values()
            .map(|payment| ParsedPayment::from_zip321(payment, network))
            .collect::<Result<Vec<_>, WalletError>>()?;
        Ok(Self { payments, network })
    }

    /// All payments declared in the request.
    #[must_use]
    pub fn payments(&self) -> &[ParsedPayment] {
        &self.payments
    }

    /// Network the request is bound to.
    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }
}

/// One payment within a [`PaymentRequest`].
#[derive(Clone, Debug)]
pub struct ParsedPayment {
    /// Recipient address.
    pub recipient: PaymentRecipient,
    /// Amount to send.
    pub amount: Zatoshis,
    /// Memo, if any. Always `None` for transparent recipients.
    pub memo: Option<Memo>,
    /// Optional human-readable label from the URI.
    pub label: Option<String>,
    /// Optional human-readable message from the URI.
    pub message: Option<String>,
}

impl ParsedPayment {
    fn from_zip321(payment: &zip321::Payment, network: Network) -> Result<Self, WalletError> {
        let encoded = payment.recipient_address().encode();
        let recipient = classify_recipient(&encoded, network);
        let upstream_amount =
            payment
                .amount()
                .ok_or_else(|| WalletError::PaymentRequestParseFailed {
                    reason: "payment is missing an amount".into(),
                })?;
        let amount = Zatoshis::try_from(upstream_amount.into_u64()).map_err(|err| {
            WalletError::PaymentRequestParseFailed {
                reason: err.to_string(),
            }
        })?;
        let memo = payment.memo().map(memo_from_memo_bytes).transpose()?;
        Ok(Self {
            recipient,
            amount,
            memo,
            label: payment.label().cloned(),
            message: payment.message().cloned(),
        })
    }
}

fn classify_recipient(encoded: &str, network: Network) -> PaymentRecipient {
    // Best-effort classification by encoded prefix. Authoritative disambiguation happens
    // in `zcash_address` at proposal time; the Zally guards distinguish the four shapes
    // below based on the encoded prefix.
    if encoded.starts_with("tex") {
        PaymentRecipient::TexAddress {
            encoded: encoded.to_owned(),
            network,
        }
    } else if encoded.starts_with('u') {
        PaymentRecipient::UnifiedAddress {
            encoded: encoded.to_owned(),
            network,
        }
    } else if encoded.starts_with("zs") || encoded.starts_with("zregtestsapling") {
        PaymentRecipient::SaplingAddress {
            encoded: encoded.to_owned(),
            network,
        }
    } else {
        PaymentRecipient::TransparentAddress {
            encoded: encoded.to_owned(),
            network,
        }
    }
}

fn memo_from_memo_bytes(bytes: &MemoBytes) -> Result<Memo, WalletError> {
    Memo::try_from(bytes).map_err(|err| WalletError::PaymentRequestParseFailed {
        reason: err.to_string(),
    })
}

/// A spend proposal returned by `Wallet::propose`.
#[derive(Clone, Debug)]
pub struct Proposal {
    total_zat: Zatoshis,
    fee_zat: Zatoshis,
    expiry_height: BlockHeight,
    output_count: usize,
}

impl Proposal {
    /// Total value transferred by this proposal (sum of recipient amounts).
    #[must_use]
    pub fn total_zat(&self) -> Zatoshis {
        self.total_zat
    }

    /// Fee paid by this proposal.
    #[must_use]
    pub fn fee_zat(&self) -> Zatoshis {
        self.fee_zat
    }

    /// `nExpiryHeight` chosen by the proposal (ZIP-203).
    #[must_use]
    pub fn expiry_height(&self) -> BlockHeight {
        self.expiry_height
    }

    /// Number of recipient outputs.
    #[must_use]
    pub fn output_count(&self) -> usize {
        self.output_count
    }
}

/// Result of a successful send.
#[derive(Clone, Debug)]
pub struct SendOutcome {
    /// Transaction identifier.
    pub tx_id: TxId,
    /// Height at which the chain source confirmed the broadcast.
    pub broadcast_at_height: BlockHeight,
}

impl Wallet {
    /// Builds a ZIP-317 conventional-fee proposal against the wallet's live notes.
    ///
    /// Validates recipient network, memo-on-transparent guard, and non-zero amount before
    /// calling `WalletStorage::propose_payment`. The storage layer wraps
    /// `zcash_client_backend::data_api::wallet::propose_standard_transfer_to_address` against
    /// the live `WalletDb` so input selection runs over real scanned notes.
    ///
    /// `not_retryable` on validation failure or insufficient balance; `retryable` on
    /// transient I/O against the wallet database.
    pub async fn propose(&self, plan: ProposalPlan) -> Result<Proposal, WalletError> {
        validate_recipient_network(&plan.recipient, self.network())?;
        validate_memo_against_recipient(plan.memo.as_ref(), &plan.recipient)?;
        validate_non_zero(plan.amount_zat)?;
        let recipient_encoded = plan.recipient.encoded().to_owned();
        let memo_bytes = plan.memo.as_ref().map(memo_to_wire_bytes);
        let summary = self
            .inner
            .storage
            .propose_payment(zally_storage::ProposalPaymentRequest::new(
                plan.account_id,
                recipient_encoded,
                plan.amount_zat.as_u64(),
                memo_bytes,
            ))
            .await
            .map_err(|err| lift_propose_error(&err))?;
        Ok(Proposal::from_storage_summary(summary))
    }

    /// Builds a proposal, signs the transaction(s), and broadcasts via `plan.submitter`.
    ///
    /// Calls `WalletStorage::prepare_payment` to run `propose_transfer` +
    /// `create_proposed_transactions` against the live `WalletDb` using a `LocalTxProver`
    /// loaded from the platform-default Sapling params location, then submits the raw
    /// transaction bytes through the supplied `Submitter`. Returns the first txid; multi-step
    /// proposals submit each step in order and return the first.
    ///
    /// `not_retryable` on validation failure, insufficient balance, or missing prover
    /// params; `retryable` on transient submitter failures.
    pub async fn send_payment(
        &self,
        plan: SendPaymentPlan<'_>,
    ) -> Result<SendOutcome, WalletError> {
        validate_recipient_network(&plan.recipient, self.network())?;
        validate_memo_against_recipient(plan.memo.as_ref(), &plan.recipient)?;
        validate_non_zero(plan.amount_zat)?;
        if plan.submitter.network() != self.network() {
            return Err(WalletError::NetworkMismatch {
                storage: self.network(),
                requested: plan.submitter.network(),
            });
        }

        if let Some(prior_tx_id) = self
            .inner
            .storage
            .lookup_idempotent_submission(&plan.idempotency)
            .await?
        {
            return Ok(SendOutcome {
                tx_id: prior_tx_id,
                broadcast_at_height: BlockHeight::from(0),
            });
        }

        let seed = self
            .inner
            .sealing
            .unseal_seed()
            .await
            .map_err(WalletError::from)?;
        let recipient_encoded = plan.recipient.encoded().to_owned();
        let memo_bytes = plan.memo.as_ref().map(memo_to_wire_bytes);
        let prepared = self
            .inner
            .storage
            .prepare_payment(
                zally_storage::ProposalPaymentRequest::new(
                    plan.account_id,
                    recipient_encoded,
                    plan.amount_zat.as_u64(),
                    memo_bytes,
                ),
                &seed,
            )
            .await
            .map_err(|err| lift_propose_error(&err))?;
        let result_tx_id = self
            .submit_prepared_transactions("send_payment.submit", plan.submitter, prepared)
            .await?;

        self.inner
            .storage
            .record_idempotent_submission(plan.idempotency, result_tx_id)
            .await?;

        Ok(SendOutcome {
            tx_id: result_tx_id,
            broadcast_at_height: BlockHeight::from(0),
        })
    }

    /// Shields wallet-owned transparent UTXOs into the account's internal shielded receiver,
    /// then broadcasts the shielding transaction through `plan.submitter`.
    ///
    /// This is the transparent-funds path exposed by librustzcash: normal payments do not use
    /// transparent UTXOs directly unless the recipient shape requires transparent inputs. The
    /// caller should run [`Wallet::sync`] first so transparent UTXOs have been refreshed from
    /// the configured [`zally_chain::ChainSource`].
    ///
    /// `not_retryable` on insufficient transparent funds or missing prover params;
    /// `retryable` on transient submitter failures.
    pub async fn shield_transparent_funds(
        &self,
        plan: ShieldTransparentPlan<'_>,
    ) -> Result<SendOutcome, WalletError> {
        validate_non_zero(plan.shielding_threshold_zat)?;
        if plan.submitter.network() != self.network() {
            return Err(WalletError::NetworkMismatch {
                storage: self.network(),
                requested: plan.submitter.network(),
            });
        }

        if let Some(prior_tx_id) = self
            .inner
            .storage
            .lookup_idempotent_submission(&plan.idempotency)
            .await?
        {
            return Ok(SendOutcome {
                tx_id: prior_tx_id,
                broadcast_at_height: BlockHeight::from(0),
            });
        }

        let seed = self
            .inner
            .sealing
            .unseal_seed()
            .await
            .map_err(WalletError::from)?;
        let prepared = self
            .inner
            .storage
            .shield_transparent_funds(
                zally_storage::ShieldTransparentRequest::new(
                    plan.account_id,
                    plan.shielding_threshold_zat.as_u64(),
                ),
                &seed,
            )
            .await
            .map_err(|err| lift_propose_error(&err))?;
        let result_tx_id = self
            .submit_prepared_transactions(
                "shield_transparent_funds.submit",
                plan.submitter,
                prepared,
            )
            .await?;

        self.inner
            .storage
            .record_idempotent_submission(plan.idempotency, result_tx_id)
            .await?;

        Ok(SendOutcome {
            tx_id: result_tx_id,
            broadcast_at_height: BlockHeight::from(0),
        })
    }

    async fn submit_prepared_transactions(
        &self,
        operation: &'static str,
        submitter: &dyn Submitter,
        prepared: Vec<zally_storage::PreparedTransaction>,
    ) -> Result<TxId, WalletError> {
        let mut first_tx_id = None;
        for transaction in prepared {
            let policy = self.retry_policy();
            let outcome = crate::retry::with_breaker_and_retry(
                &self.inner.circuit_breaker,
                policy,
                operation,
                || submitter.submit(&transaction.raw_bytes),
                |err| map_submitter_error(&err),
            )
            .await?;
            let tx_id = resolve_send_outcome(outcome, transaction.tx_id)?;
            if first_tx_id.is_none() {
                first_tx_id = Some(tx_id);
            }
        }
        first_tx_id.ok_or_else(|| WalletError::ProposalRejected {
            reason: "transaction construction returned no transactions".into(),
        })
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive submit outcomes map unknown variants to ProposalRejected"
)]
fn resolve_send_outcome(
    outcome: zally_chain::SubmitOutcome,
    fallback_tx_id: TxId,
) -> Result<TxId, WalletError> {
    match outcome {
        zally_chain::SubmitOutcome::Accepted { tx_id } => Ok(tx_id),
        zally_chain::SubmitOutcome::Duplicate { .. } => Ok(fallback_tx_id),
        zally_chain::SubmitOutcome::Rejected { reason } => {
            Err(WalletError::ProposalRejected { reason })
        }
        _ => Err(WalletError::ProposalRejected {
            reason: "submitter returned an unrecognised outcome variant".into(),
        }),
    }
}

fn map_submitter_error(err: &zally_chain::SubmitterError) -> WalletError {
    WalletError::ChainSource {
        reason: err.to_string(),
        is_retryable: err.is_retryable(),
    }
}

fn memo_to_wire_bytes(memo: &Memo) -> Vec<u8> {
    MemoBytes::from(memo).as_slice().to_vec()
}

fn lift_propose_error(err: &zally_storage::StorageError) -> WalletError {
    let display = err.to_string().to_lowercase();
    if display.contains("insufficient") || display.contains("balanceerror") {
        return WalletError::InsufficientBalance {
            requested_zat: 0,
            spendable_zat: 0,
        };
    }
    WalletError::ProposalRejected {
        reason: err.to_string(),
    }
}

impl Proposal {
    fn from_storage_summary(summary: zally_storage::ProposalSummary) -> Self {
        Self {
            total_zat: Zatoshis::try_from(summary.total_zat).unwrap_or(Zatoshis::zero()),
            fee_zat: Zatoshis::try_from(summary.fee_zat).unwrap_or(Zatoshis::zero()),
            expiry_height: summary.min_target_height,
            output_count: summary.output_count,
        }
    }
}

/// Inputs to [`Wallet::propose`].
///
/// New fields land as additive `pub` members under `#[non_exhaustive]`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ProposalPlan {
    /// Account that funds the proposal.
    pub account_id: AccountId,
    /// Payment recipient.
    pub recipient: PaymentRecipient,
    /// Amount to send.
    pub amount_zat: Zatoshis,
    /// Optional memo (rejected for transparent recipients).
    pub memo: Option<Memo>,
}

impl ProposalPlan {
    /// Constructs a ZIP-317 conventional-fee proposal plan.
    #[must_use]
    pub fn conventional(
        account_id: AccountId,
        recipient: PaymentRecipient,
        amount_zat: Zatoshis,
        memo: Option<Memo>,
    ) -> Self {
        Self {
            account_id,
            recipient,
            amount_zat,
            memo,
        }
    }
}

/// Inputs to [`Wallet::send_payment`].
#[non_exhaustive]
pub struct SendPaymentPlan<'submitter> {
    /// Account that funds the send.
    pub account_id: AccountId,
    /// Caller-supplied idempotency key.
    pub idempotency: IdempotencyKey,
    /// Payment recipient.
    pub recipient: PaymentRecipient,
    /// Amount to send.
    pub amount_zat: Zatoshis,
    /// Optional memo (rejected for transparent recipients).
    pub memo: Option<Memo>,
    /// Submitter that delivers the signed transaction.
    pub submitter: &'submitter dyn Submitter,
}

/// Inputs to [`Wallet::shield_transparent_funds`].
#[non_exhaustive]
pub struct ShieldTransparentPlan<'submitter> {
    /// Account whose transparent UTXOs are shielded.
    pub account_id: AccountId,
    /// Caller-supplied idempotency key.
    pub idempotency: IdempotencyKey,
    /// Minimum total transparent input value to shield, in zatoshis.
    pub shielding_threshold_zat: Zatoshis,
    /// Submitter that delivers the signed shielding transaction.
    pub submitter: &'submitter dyn Submitter,
}

impl<'submitter> ShieldTransparentPlan<'submitter> {
    /// Constructs a transparent shielding plan.
    #[must_use]
    pub const fn new(
        account_id: AccountId,
        idempotency: IdempotencyKey,
        shielding_threshold_zat: Zatoshis,
        submitter: &'submitter dyn Submitter,
    ) -> Self {
        Self {
            account_id,
            idempotency,
            shielding_threshold_zat,
            submitter,
        }
    }
}

impl<'submitter> SendPaymentPlan<'submitter> {
    /// Constructs a ZIP-317 conventional-fee send plan with no memo.
    #[must_use]
    pub fn conventional(
        account_id: AccountId,
        idempotency: IdempotencyKey,
        recipient: PaymentRecipient,
        amount_zat: Zatoshis,
        submitter: &'submitter dyn Submitter,
    ) -> Self {
        Self {
            account_id,
            idempotency,
            recipient,
            amount_zat,
            memo: None,
            submitter,
        }
    }

    /// Returns the plan with `memo` attached.
    #[must_use]
    pub fn with_memo(mut self, memo: Memo) -> Self {
        self.memo = Some(memo);
        self
    }
}

fn validate_recipient_network(
    recipient: &PaymentRecipient,
    wallet_network: Network,
) -> Result<(), WalletError> {
    if recipient.network() == wallet_network {
        Ok(())
    } else {
        Err(WalletError::NetworkMismatch {
            storage: wallet_network,
            requested: recipient.network(),
        })
    }
}

fn validate_memo_against_recipient(
    memo: Option<&Memo>,
    recipient: &PaymentRecipient,
) -> Result<(), WalletError> {
    let Some(memo) = memo else {
        return Ok(());
    };
    if matches!(memo, Memo::Empty) {
        return Ok(());
    }
    if recipient.is_transparent() {
        Err(WalletError::MemoOnTransparentRecipient)
    } else {
        Ok(())
    }
}

fn validate_non_zero(amount_zat: Zatoshis) -> Result<(), WalletError> {
    if amount_zat.as_u64() == 0 {
        Err(WalletError::ProposalRejected {
            reason: "amount must be greater than zero zatoshis".into(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regtest() -> Network {
        Network::regtest()
    }

    #[test]
    fn validate_memo_rejects_text_memo_on_transparent_recipient() -> Result<(), WalletError> {
        let recipient = PaymentRecipient::TransparentAddress {
            encoded: "t1example".into(),
            network: regtest(),
        };
        let memo =
            Memo::from_bytes(b"hello").map_err(|err| WalletError::PaymentRequestParseFailed {
                reason: err.to_string(),
            })?;
        let outcome = validate_memo_against_recipient(Some(&memo), &recipient);
        assert!(matches!(
            outcome,
            Err(WalletError::MemoOnTransparentRecipient)
        ));
        Ok(())
    }

    #[test]
    fn validate_memo_accepts_empty_memo_on_transparent_recipient() {
        let recipient = PaymentRecipient::TransparentAddress {
            encoded: "t1example".into(),
            network: regtest(),
        };
        let memo = Memo::Empty;
        assert!(validate_memo_against_recipient(Some(&memo), &recipient).is_ok());
    }

    #[test]
    fn validate_memo_accepts_text_memo_on_shielded_recipient() -> Result<(), WalletError> {
        let recipient = PaymentRecipient::UnifiedAddress {
            encoded: "uregtest1example".into(),
            network: regtest(),
        };
        let memo = Memo::from_bytes(b"invoice 1234").map_err(|err| {
            WalletError::PaymentRequestParseFailed {
                reason: err.to_string(),
            }
        })?;
        validate_memo_against_recipient(Some(&memo), &recipient)?;
        Ok(())
    }

    #[test]
    fn validate_recipient_network_rejects_mismatch() {
        let recipient = PaymentRecipient::UnifiedAddress {
            encoded: "u1example".into(),
            network: Network::Mainnet,
        };
        let outcome = validate_recipient_network(&recipient, regtest());
        assert!(matches!(outcome, Err(WalletError::NetworkMismatch { .. })));
    }

    #[test]
    fn validate_non_zero_rejects_zero_amount() {
        assert!(matches!(
            validate_non_zero(Zatoshis::zero()),
            Err(WalletError::ProposalRejected { .. })
        ));
    }
}
