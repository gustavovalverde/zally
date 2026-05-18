//! Spend flow: payment-request parsing, proposal, send.
//!
//! Carries the ZIP-302, ZIP-320, and ZIP-321 guards on the wallet layer. `Wallet::propose`
//! drives `propose_standard_transfer_to_address` against the live `WalletDb`;
//! `Wallet::send_payment` adds signing and submission through the caller-supplied
//! `Submitter`.

use std::collections::HashSet;

use zally_chain::Submitter;
use zally_core::{
    AccountId, BlockHeight, IdempotencyKey, Memo, MemoBytes, Network, OutPoint, PaymentRecipient,
    TxId, Zatoshis,
};

use crate::wallet::{Wallet, current_unix_ms};
use crate::wallet_error::WalletError;

/// Submit-time context bundled per spend call.
///
/// Holds the operator-label, the submitter, the account being spent from, and the
/// wallet's current observed tip so submit-time helpers can stay under the clippy
/// argument-count limit.
struct SubmissionContext<'a> {
    operation: &'static str,
    submitter: &'a dyn Submitter,
    account_id: AccountId,
    observed_tip: Option<BlockHeight>,
}

/// Pending-broadcast row contents derived from one prepared transaction.
struct PendingBroadcastSnapshot {
    broadcast_tx_id: TxId,
    broadcast_at_ms: u64,
    observed_tip: Option<BlockHeight>,
    inputs: Vec<(OutPoint, Zatoshis)>,
}

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
                plan.amount_zat,
                memo_bytes,
            ))
            .await
            .map_err(WalletError::from)?;
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
            .find_idempotent_submission(&plan.idempotency)
            .await?
        {
            return Ok(SendOutcome {
                tx_id: prior_tx_id,
                broadcast_at_height: self.observed_tip_or_zero().await?,
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
        let excluded_outpoints = self
            .collect_pending_broadcast_outpoints(plan.account_id)
            .await?;
        let prepared = self
            .inner
            .storage
            .prepare_payment(
                zally_storage::ProposalPaymentRequest::new(
                    plan.account_id,
                    recipient_encoded,
                    plan.amount_zat,
                    memo_bytes,
                ),
                excluded_outpoints,
                &seed,
            )
            .await
            .map_err(WalletError::from)?;
        let observed_tip = self.inner.storage.find_observed_tip().await?;
        let result_tx_id = self
            .submit_and_record_pending(
                SubmissionContext {
                    operation: "send_payment.submit",
                    submitter: plan.submitter,
                    account_id: plan.account_id,
                    observed_tip,
                },
                prepared,
            )
            .await?;

        self.inner
            .storage
            .record_idempotent_submission(plan.idempotency, result_tx_id)
            .await?;

        Ok(SendOutcome {
            tx_id: result_tx_id,
            broadcast_at_height: observed_tip.unwrap_or_else(|| BlockHeight::from(0)),
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
            .find_idempotent_submission(&plan.idempotency)
            .await?
        {
            return Ok(SendOutcome {
                tx_id: prior_tx_id,
                broadcast_at_height: self.observed_tip_or_zero().await?,
            });
        }

        let seed = self
            .inner
            .sealing
            .unseal_seed()
            .await
            .map_err(WalletError::from)?;
        let excluded_outpoints = self
            .collect_pending_broadcast_outpoints(plan.account_id)
            .await?;
        let prepared = self
            .inner
            .storage
            .shield_transparent_funds(
                zally_storage::ShieldTransparentRequest::new(
                    plan.account_id,
                    plan.shielding_threshold_zat,
                ),
                excluded_outpoints,
                &seed,
            )
            .await
            .map_err(WalletError::from)?;
        let observed_tip = self.inner.storage.find_observed_tip().await?;
        let result_tx_id = self
            .submit_and_record_pending(
                SubmissionContext {
                    operation: "shield_transparent_funds.submit",
                    submitter: plan.submitter,
                    account_id: plan.account_id,
                    observed_tip,
                },
                prepared,
            )
            .await?;

        self.inner
            .storage
            .record_idempotent_submission(plan.idempotency, result_tx_id)
            .await?;

        Ok(SendOutcome {
            tx_id: result_tx_id,
            broadcast_at_height: observed_tip.unwrap_or_else(|| BlockHeight::from(0)),
        })
    }

    /// Submits each prepared transaction, recording its outpoints in the pending-broadcast
    /// filter **before** the submit call so a crash between submit-accept and
    /// row-persistence cannot leave an in-flight broadcast unrecorded. On submit rejection
    /// the just-recorded row is removed so the outpoints become spendable again.
    async fn submit_and_record_pending(
        &self,
        submission: SubmissionContext<'_>,
        prepared: Vec<zally_storage::PreparedTransaction>,
    ) -> Result<TxId, WalletError> {
        let mut first_tx_id = None;
        let broadcast_at_ms = current_unix_ms();
        for transaction in prepared {
            self.record_broadcast_inputs(
                submission.account_id,
                PendingBroadcastSnapshot {
                    broadcast_tx_id: transaction.tx_id,
                    broadcast_at_ms,
                    observed_tip: submission.observed_tip,
                    inputs: transaction.transparent_inputs.clone(),
                },
            )
            .await?;

            let policy = self.retry_policy();
            let outcome = crate::retry::with_breaker_and_retry(
                &self.inner.circuit_breaker,
                policy,
                submission.operation,
                || submission.submitter.submit(&transaction.raw_bytes),
                WalletError::from,
            )
            .await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.clear_pending_after_failed_submit(transaction.tx_id)
                        .await;
                    return Err(err);
                }
            };
            let tx_id = match resolve_send_outcome(outcome, transaction.tx_id) {
                Ok(tx_id) => tx_id,
                Err(err) => {
                    self.clear_pending_after_failed_submit(transaction.tx_id)
                        .await;
                    return Err(err);
                }
            };
            if first_tx_id.is_none() {
                first_tx_id = Some(tx_id);
            }
        }
        first_tx_id.ok_or_else(|| WalletError::ProposalRejected {
            reason: "transaction construction returned no transactions".into(),
        })
    }

    async fn record_broadcast_inputs(
        &self,
        account_id: AccountId,
        snapshot: PendingBroadcastSnapshot,
    ) -> Result<(), WalletError> {
        if snapshot.inputs.is_empty() {
            return Ok(());
        }
        self.inner
            .storage
            .record_pending_broadcast_inputs(zally_storage::PendingBroadcastRecord {
                broadcast_tx_id: snapshot.broadcast_tx_id,
                account_id,
                broadcast_at_ms: snapshot.broadcast_at_ms,
                broadcast_at_height: snapshot.observed_tip,
                inputs: snapshot.inputs,
            })
            .await
            .map_err(WalletError::from)
    }

    async fn clear_pending_after_failed_submit(&self, broadcast_tx_id: TxId) {
        let _ = self
            .inner
            .storage
            .clear_pending_broadcast_inputs_for_mined(&[broadcast_tx_id])
            .await;
    }

    async fn collect_pending_broadcast_outpoints(
        &self,
        account_id: AccountId,
    ) -> Result<HashSet<OutPoint>, WalletError> {
        let after_at_ms = self.pending_broadcast_cutoff_ms();
        let rows = self
            .inner
            .storage
            .list_pending_broadcast_inputs(account_id, after_at_ms)
            .await?;
        Ok(rows.into_iter().map(|row| row.outpoint).collect())
    }

    /// Unix millisecond cutoff used by every pending-broadcast read: rows older than this
    /// fall outside the operator-configured inflight window. Centralized so the three call
    /// sites (`Wallet::get_pending_transparent_inputs`, the spend filter, and the sync
    /// cleanup) cannot drift.
    pub(crate) fn pending_broadcast_cutoff_ms(&self) -> u64 {
        current_unix_ms().saturating_sub(self.inner.options.pending_broadcast_window_ms)
    }

    async fn observed_tip_or_zero(&self) -> Result<BlockHeight, WalletError> {
        Ok(self
            .inner
            .storage
            .find_observed_tip()
            .await?
            .unwrap_or_else(|| BlockHeight::from(0)))
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

fn memo_to_wire_bytes(memo: &Memo) -> Vec<u8> {
    MemoBytes::from(memo).as_slice().to_vec()
}

impl Proposal {
    fn from_storage_summary(summary: zally_storage::ProposalSummary) -> Self {
        Self {
            total_zat: summary.total_zat,
            fee_zat: summary.fee_zat,
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
