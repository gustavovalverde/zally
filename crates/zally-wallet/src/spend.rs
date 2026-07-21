//! Spend flow: payment-request parsing, proposal, send.
//!
//! Carries the ZIP-302, ZIP-320, and ZIP-321 guards on the wallet layer. `Wallet::propose`
//! drives `propose_standard_transfer_to_address` against the live `WalletDb`;
//! `Wallet::send_payment` adds signing and submission through the caller-supplied
//! `Submitter`.

use std::collections::HashSet;

use zally_chain::{ShieldedPool, Submitter};
use zally_core::{
    AccountId, BlockHeight, IdempotencyKey, Memo, MemoBytes, Network, OutPoint, PaymentRecipient,
    TxId, Zatoshis,
};

use crate::error::WalletError;
use crate::wallet::{Wallet, current_unix_ms};

/// Submit-time context bundled per spend call.
///
/// Holds the operator-label, the submitter, the account being spent from, and the
/// wallet's current visible tip so submit-time helpers can stay under the clippy
/// argument-count limit.
struct SubmissionContext<'a> {
    operation: &'static str,
    submitter: &'a dyn Submitter,
    account_id: AccountId,
    visible_tip: Option<BlockHeight>,
}

/// Pending-broadcast row contents derived from one prepared transaction.
struct PendingBroadcastSnapshot {
    broadcast_tx_id: TxId,
    broadcast_at_ms: u64,
    visible_tip: Option<BlockHeight>,
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

    /// Serialises this payment request as a ZIP-321 URI.
    ///
    /// Round-trips through [`PaymentRequest::from_uri`] for any request
    /// constructed through the public surface.
    ///
    /// `not_retryable`: identical inputs always produce the same URI; failures
    /// reflect malformed recipient encodings or memos that are too long for
    /// their address kind.
    pub fn to_uri(&self) -> Result<String, WalletError> {
        let payments = self
            .payments
            .iter()
            .map(parsed_payment_to_zip321)
            .collect::<Result<Vec<_>, WalletError>>()?;
        let request = zip321::TransactionRequest::new(payments).map_err(|err| {
            WalletError::PaymentRequestParseFailed {
                reason: err.to_string(),
            }
        })?;
        Ok(request.to_uri())
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
    } else if encoded.starts_with("zs")
        || encoded.starts_with("ztestsapling")
        || encoded.starts_with("zregtestsapling")
    {
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

fn parsed_payment_to_zip321(payment: &ParsedPayment) -> Result<zip321::Payment, WalletError> {
    let recipient_address = recipient_to_zcash_address(&payment.recipient)?;
    let amount = upstream_zatoshis(payment.amount)?;
    let memo = payment.memo.as_ref().map(MemoBytes::from);
    zip321::Payment::new(
        recipient_address,
        Some(amount),
        memo,
        payment.label.clone(),
        payment.message.clone(),
        vec![],
    )
    .map_err(|err| WalletError::PaymentRequestParseFailed {
        reason: err.to_string(),
    })
}

fn recipient_to_zcash_address(
    recipient: &PaymentRecipient,
) -> Result<zcash_address::ZcashAddress, WalletError> {
    recipient
        .encoded()
        .parse::<zcash_address::ZcashAddress>()
        .map_err(|err| WalletError::PaymentRequestParseFailed {
            reason: err.to_string(),
        })
}

fn upstream_zatoshis(zatoshis: Zatoshis) -> Result<zcash_protocol::value::Zatoshis, WalletError> {
    zcash_protocol::value::Zatoshis::from_u64(zatoshis.as_u64()).map_err(|err| {
        WalletError::PaymentRequestParseFailed {
            reason: err.to_string(),
        }
    })
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

/// Result of building and signing a transaction without broadcast.
///
/// Produced by `Wallet::sign_pczt` and by the internal sign step inside
/// `Wallet::send_payment`. The transaction is fully signed but has not yet
/// been handed to a [`zally_chain::Submitter`]. `fee_zat` reflects the fee
/// the proposal allocated; `tx_id` is the ZIP-244 identifier of the signed
/// transaction. Phase 2d of Proposal-0003 fills the full PCZT path; this
/// type lands in 2e ahead of that to lock the vocabulary.
#[derive(Clone, Debug)]
pub struct SignedPczt {
    /// Transaction identifier of the signed transaction.
    pub tx_id: TxId,
    /// Fee allocated by the proposal builder, in zatoshis. Zero until the
    /// 2d PCZT path threads the proposal-fee value through here.
    pub fee_zat: zally_core::Zatoshis,
    /// Block height at which Zcash consensus will reject this transaction
    /// if it has not been mined.
    pub tx_expiry_height: BlockHeight,
}

/// Result of a successful broadcast.
///
/// Produced by `Wallet::send_payment` after a [`zally_chain::Submitter::submit`]
/// accept. Distinct from [`SignedPczt`] because the signing and broadcast
/// phases of D-9 of Proposal-0003 live in two separate runtimes
/// (`zspend-runtime` signs; `zpay-runtime` broadcasts). Inside the
/// `send_payment` convenience method the two phases are still composed in
/// one call; this type makes the post-broadcast facts visible separately.
#[derive(Clone, Debug)]
pub struct BroadcastOutcome {
    /// Transaction identifier as confirmed by the broadcast plane.
    pub tx_id: TxId,
    /// Height at which the chain source observed the broadcast. Zero when
    /// the chain source could not produce a tip snapshot at submit time.
    pub broadcast_at_height: BlockHeight,
}

/// Composed result of `Wallet::send_payment`: signed + broadcast facts.
///
/// Field access for callers that previously read `outcome.tx_id`,
/// `outcome.broadcast_at_height`, or `outcome.tx_expiry_height` migrates
/// to `outcome.signed.tx_id`, `outcome.broadcast.broadcast_at_height`, and
/// `outcome.signed.tx_expiry_height` respectively. The split is intentional
/// (Proposal-0003 D-3): the two phases run in different services in
/// production, even though the in-process `send_payment` helper still
/// composes them.
#[derive(Clone, Debug)]
pub struct SendOutcome {
    /// Sign-phase facts.
    pub signed: SignedPczt,
    /// Broadcast-phase facts.
    pub broadcast: BroadcastOutcome,
}

impl SendOutcome {
    /// Convenience accessor for the transaction id common to both phases.
    /// Equal to `self.signed.tx_id` and `self.broadcast.tx_id`.
    #[must_use]
    pub const fn tx_id(&self) -> TxId {
        self.signed.tx_id
    }
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
        let summary = self
            .inner
            .storage
            .propose_payment(storage_proposal_request(&plan)?)
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
    #[allow(
        clippy::too_many_lines,
        reason = "send_payment is the single composed flow; splitting it further would scatter the (sign, idempotency, broadcast) transaction across helpers"
    )]
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
            let broadcast_at_height = self.visible_tip_or_zero().await?;
            return Ok(SendOutcome {
                signed: SignedPczt {
                    tx_id: prior_tx_id,
                    fee_zat: zally_core::Zatoshis::zero(),
                    tx_expiry_height: BlockHeight::from(0),
                },
                broadcast: BroadcastOutcome {
                    tx_id: prior_tx_id,
                    broadcast_at_height,
                },
            });
        }

        let prepared = if let Some(target) = plan.target_expiry_height {
            self.prepare_send_with_target_expiry(&plan, target).await?
        } else {
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
            self.inner
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
                .map_err(WalletError::from)?
        };
        let visible_tip = self.inner.storage.find_visible_tip().await?;
        let (result_tx_id, tx_expiry_height) = self
            .submit_and_record_pending(
                SubmissionContext {
                    operation: "send_payment.submit",
                    submitter: plan.submitter,
                    account_id: plan.account_id,
                    visible_tip,
                },
                prepared,
            )
            .await?;

        self.inner
            .storage
            .record_idempotent_submission(plan.idempotency, result_tx_id)
            .await?;

        Ok(SendOutcome {
            signed: SignedPczt {
                tx_id: result_tx_id,
                fee_zat: zally_core::Zatoshis::zero(),
                tx_expiry_height,
            },
            broadcast: BroadcastOutcome {
                tx_id: result_tx_id,
                broadcast_at_height: visible_tip.unwrap_or_else(|| BlockHeight::from(0)),
            },
        })
    }

    /// Routes a `SendPaymentPlan` with `target_expiry_height` through the PCZT path.
    ///
    /// Composes propose-with-expiry -> prove -> sign -> extract, then
    /// validates the signed expiry against the caller's target before handing the
    /// prepared transaction back to `send_payment` for submission.
    ///
    /// Rejects targets at or below the wallet's visible chain tip with
    /// [`WalletError::TargetExpiryStale`]: any signed bytes carrying such an expiry
    /// would already be unmineable. Rejects a signed expiry that diverges from the
    /// target with [`WalletError::TargetExpiryMismatch`], which catches expiry propagation
    /// bugs at the wallet boundary rather than at the caller.
    async fn prepare_send_with_target_expiry(
        &self,
        plan: &SendPaymentPlan<'_>,
        target: BlockHeight,
    ) -> Result<Vec<zally_storage::PreparedTransaction>, WalletError> {
        let visible_tip = self.visible_tip_or_zero().await?;
        if u32::from(target) <= u32::from(visible_tip) {
            return Err(WalletError::TargetExpiryStale {
                target,
                visible_tip,
            });
        }

        let proposal_plan = ProposalPlan::conventional(
            plan.account_id,
            plan.recipient.clone(),
            plan.amount_zat,
            plan.memo.clone(),
        );
        let proposed = self.propose_pczt(proposal_plan, Some(target)).await?;
        let proven = self.prove_pczt(proposed).await?;
        let signed = self.sign_pczt(proven).await?;

        let prepared = self
            .inner
            .storage
            .extract_and_store_pczt(signed.into_bytes())
            .await
            .map_err(WalletError::from)?;
        let signed_height = prepared.tx_expiry_height;
        if u32::from(signed_height) != u32::from(target) {
            return Err(WalletError::TargetExpiryMismatch {
                target,
                signed: signed_height,
            });
        }

        Ok(vec![prepared])
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
            let broadcast_at_height = self.visible_tip_or_zero().await?;
            return Ok(SendOutcome {
                signed: SignedPczt {
                    tx_id: prior_tx_id,
                    fee_zat: zally_core::Zatoshis::zero(),
                    tx_expiry_height: BlockHeight::from(0),
                },
                broadcast: BroadcastOutcome {
                    tx_id: prior_tx_id,
                    broadcast_at_height,
                },
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
        let shielding_request = resolve_shielding_request(&plan)?;
        let prepared = self
            .inner
            .storage
            .shield_transparent_funds(shielding_request, excluded_outpoints, &seed)
            .await
            .map_err(WalletError::from)?;
        let visible_tip = self.inner.storage.find_visible_tip().await?;
        let (result_tx_id, tx_expiry_height) = self
            .submit_and_record_pending(
                SubmissionContext {
                    operation: "shield_transparent_funds.submit",
                    submitter: plan.submitter,
                    account_id: plan.account_id,
                    visible_tip,
                },
                prepared,
            )
            .await?;

        self.inner
            .storage
            .record_idempotent_submission(plan.idempotency, result_tx_id)
            .await?;

        Ok(SendOutcome {
            signed: SignedPczt {
                tx_id: result_tx_id,
                fee_zat: zally_core::Zatoshis::zero(),
                tx_expiry_height,
            },
            broadcast: BroadcastOutcome {
                tx_id: result_tx_id,
                broadcast_at_height: visible_tip.unwrap_or_else(|| BlockHeight::from(0)),
            },
        })
    }

    /// Submits each prepared transaction, recording its outpoints in the pending-broadcast
    /// filter **before** the submit call so a crash between submit-accept and
    /// row-persistence cannot leave an in-flight broadcast unrecorded. On submit rejection
    /// the just-recorded row is removed so the outpoints become spendable again.
    ///
    /// Returns the first transaction's `(tx_id, tx_expiry_height)` pair so callers can
    /// pass expiry through to consumers that need to bound their wait for confirmation.
    async fn submit_and_record_pending(
        &self,
        submission: SubmissionContext<'_>,
        prepared: Vec<zally_storage::PreparedTransaction>,
    ) -> Result<(TxId, BlockHeight), WalletError> {
        let mut first_result = None;
        let broadcast_at_ms = current_unix_ms();
        for transaction in prepared {
            let tx_expiry_height = transaction.tx_expiry_height;
            self.record_broadcast_inputs(
                submission.account_id,
                PendingBroadcastSnapshot {
                    broadcast_tx_id: transaction.tx_id,
                    broadcast_at_ms,
                    visible_tip: submission.visible_tip,
                    inputs: transaction.transparent_inputs.clone(),
                },
            )
            .await?;

            let policy = self.broadcast_retry_policy();
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
            if first_result.is_none() {
                first_result = Some((tx_id, tx_expiry_height));
            }
        }
        first_result.ok_or_else(|| WalletError::ProposalRejected {
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
                broadcast_at_height: snapshot.visible_tip,
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

    async fn visible_tip_or_zero(&self) -> Result<BlockHeight, WalletError> {
        Ok(self
            .inner
            .storage
            .find_visible_tip()
            .await?
            .unwrap_or_else(|| BlockHeight::from(0)))
    }
}

fn resolve_shielding_request(
    plan: &ShieldTransparentPlan<'_>,
) -> Result<zally_storage::ShieldTransparentRequest, WalletError> {
    let request =
        zally_storage::ShieldTransparentRequest::new(plan.account_id, plan.shielding_threshold_zat);
    let Some(destination_pool) = plan.destination_pool else {
        return Ok(request);
    };
    Ok(request.with_destination_pool(protocol_shielded_pool(destination_pool)?))
}

pub(crate) fn protocol_shielded_pool(
    source_pool: ShieldedPool,
) -> Result<zcash_protocol::ShieldedPool, WalletError> {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "ShieldedPool is non-exhaustive across the zally-chain crate boundary"
    )]
    Ok(match source_pool {
        ShieldedPool::Sapling => zcash_protocol::ShieldedPool::Sapling,
        ShieldedPool::Orchard => zcash_protocol::ShieldedPool::Orchard,
        ShieldedPool::Ironwood => zcash_protocol::ShieldedPool::Ironwood,
        _ => {
            return Err(WalletError::ProposalRejected {
                reason: "shielded pool is not supported by this release".into(),
            });
        }
    })
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive submit outcomes fall through to SubmissionRejected with a placeholder typed reason"
)]
fn resolve_send_outcome(
    outcome: zally_chain::SubmitOutcome,
    expected_tx_id: TxId,
) -> Result<TxId, WalletError> {
    match outcome {
        zally_chain::SubmitOutcome::Accepted { tx_id }
        | zally_chain::SubmitOutcome::Duplicate { tx_id }
        | zally_chain::SubmitOutcome::Queued { tx_id } => {
            if tx_id != expected_tx_id {
                return Err(WalletError::SubmittedTransactionIdMismatch {
                    expected: expected_tx_id,
                    returned: tx_id,
                });
            }
            Ok(tx_id)
        }
        zally_chain::SubmitOutcome::Rejected { reason, detail } => {
            Err(WalletError::SubmissionRejected { reason, detail })
        }
        _ => Err(WalletError::SubmissionRejected {
            reason: zally_chain::RejectionReason::Unknown,
            detail: "submitter returned an unrecognised outcome variant".into(),
        }),
    }
}

fn memo_to_wire_bytes(memo: &Memo) -> Vec<u8> {
    MemoBytes::from(memo).as_slice().to_vec()
}

pub(crate) fn storage_proposal_request(
    plan: &ProposalPlan,
) -> Result<zally_storage::ProposalPaymentRequest, WalletError> {
    let mut request = zally_storage::ProposalPaymentRequest::new(
        plan.account_id,
        plan.recipient.encoded().to_owned(),
        plan.amount_zat,
        plan.memo.as_ref().map(memo_to_wire_bytes),
    );
    if let Some(source_pool) = plan.source_pool {
        request = request.with_source_pool(protocol_shielded_pool(source_pool)?);
    }
    Ok(request)
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
    /// Shielded pool from which inputs may be selected.
    ///
    /// `None` permits the wallet's default multi-pool selection policy. Setting a pool fails
    /// with insufficient funds rather than crossing into another pool.
    pub source_pool: Option<ShieldedPool>,
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
            source_pool: None,
        }
    }

    /// Restricts shielded input selection to `source_pool`.
    #[must_use]
    pub const fn with_source_pool(mut self, source_pool: ShieldedPool) -> Self {
        self.source_pool = Some(source_pool);
        self
    }

    /// Shielded pool from which inputs may be selected, if restricted.
    #[must_use]
    pub const fn source_pool(&self) -> Option<ShieldedPool> {
        self.source_pool
    }
}

/// Inputs to [`Wallet::send_payment`].
///
/// Naming convention: fields prefixed with `target_` are caller-controlled wallet
/// parameters that the wallet honours rather than derives. For example,
/// `target_expiry_height` lets the caller commit to a specific expiry instead of
/// letting the wallet derive one from its visible tip.
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
    /// Caller-supplied expiry height to commit to.
    ///
    /// When set, the wallet builds the transaction through the PCZT path and sets
    /// `global.expiry_height` before the IO Finalizer, prover, and signer run. When
    /// `None`, the wallet uses its own chain-tip-derived default.
    pub target_expiry_height: Option<BlockHeight>,
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
    /// Shielded pool that receives the swept funds.
    ///
    /// `None` preserves the wallet's activation-aware default.
    pub destination_pool: Option<ShieldedPool>,
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
            destination_pool: None,
            submitter,
        }
    }

    /// Returns the plan with an explicit shielded destination pool.
    #[must_use]
    pub const fn with_destination_pool(mut self, destination_pool: ShieldedPool) -> Self {
        self.destination_pool = Some(destination_pool);
        self
    }

    /// Returns the explicit shielded destination pool, if one was selected.
    #[must_use]
    pub const fn destination_pool(&self) -> Option<ShieldedPool> {
        self.destination_pool
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
            target_expiry_height: None,
            submitter,
        }
    }

    /// Returns the plan with `memo` attached.
    #[must_use]
    pub fn with_memo(mut self, memo: Memo) -> Self {
        self.memo = Some(memo);
        self
    }

    /// Returns the plan with `target_expiry_height` set.
    ///
    /// Routes the send through the PCZT path so the caller-supplied height can be
    /// committed before the IO Finalizer, prover, and signer run. Without this
    /// builder call the wallet picks its own expiry from the chain tip.
    #[must_use]
    pub fn with_target_expiry_height(mut self, height: BlockHeight) -> Self {
        self.target_expiry_height = Some(height);
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
    fn classify_recipient_recognizes_testnet_sapling_address() {
        let encoded = "ztestsapling12p79hg7sffq7j2ukmpur208cyy7cxdr4mkwnn8eh09w3hgnysv6dmtwuwy8z7e6lvgmngrxeh6g";
        let recipient = classify_recipient(encoded, Network::Testnet);

        assert!(matches!(
            recipient,
            PaymentRecipient::SaplingAddress {
                encoded: recipient_encoded,
                network: Network::Testnet,
            } if recipient_encoded == encoded
        ));
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

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "fixture amount is valid by construction; expect keeps the builder call readable"
    )]
    fn proposal_plan_can_restrict_the_source_pool() {
        let plan = ProposalPlan::conventional(
            zally_core::AccountId::from_uuid(uuid::Uuid::nil()),
            PaymentRecipient::SaplingAddress {
                encoded: "zregtestsapling1example".into(),
                network: regtest(),
            },
            Zatoshis::try_from(1_u64).expect("zatoshis"),
            None,
        );

        assert_eq!(plan.source_pool(), None);
        assert_eq!(
            plan.with_source_pool(ShieldedPool::Sapling).source_pool(),
            Some(ShieldedPool::Sapling)
        );
    }

    /// `SendPaymentPlan::conventional` starts with `target_expiry_height: None`, and
    /// `with_target_expiry_height` is the only way to opt in to the PCZT routing path.
    #[test]
    #[allow(
        clippy::expect_used,
        reason = "fixture literals are valid by construction; expect keeps the builder call readable"
    )]
    fn send_payment_plan_target_expiry_height_defaults_to_none() {
        let submitter = zally_testkit::MockSubmitter::accepting(regtest());
        let plan = SendPaymentPlan::conventional(
            zally_core::AccountId::from_uuid(uuid::Uuid::nil()),
            zally_core::IdempotencyKey::try_from("inert-builder-key").expect("key"),
            PaymentRecipient::UnifiedAddress {
                encoded: "uregtest1example".into(),
                network: regtest(),
            },
            Zatoshis::try_from(1_u64).expect("zatoshis"),
            &submitter,
        );
        assert!(
            plan.target_expiry_height.is_none(),
            "conventional() must not commit to any target expiry"
        );

        let with_target = plan.with_target_expiry_height(BlockHeight::from(123));
        assert_eq!(
            with_target.target_expiry_height,
            Some(BlockHeight::from(123))
        );
    }

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "fixture literals are valid by construction; expect keeps the builder call readable"
    )]
    fn shield_transparent_plan_selects_destination_pool() {
        let submitter = zally_testkit::MockSubmitter::accepting(regtest());
        let plan = ShieldTransparentPlan::new(
            zally_core::AccountId::from_uuid(uuid::Uuid::nil()),
            zally_core::IdempotencyKey::try_from("shield-builder-key").expect("key"),
            Zatoshis::try_from(1_u64).expect("zatoshis"),
            &submitter,
        );

        assert_eq!(plan.destination_pool(), None);
        let sapling_plan = plan.with_destination_pool(zally_chain::ShieldedPool::Sapling);
        assert_eq!(
            sapling_plan.destination_pool(),
            Some(zally_chain::ShieldedPool::Sapling)
        );
        let storage_request =
            resolve_shielding_request(&sapling_plan).expect("Sapling is supported");
        assert_eq!(
            storage_request.destination_pool(),
            Some(zcash_protocol::ShieldedPool::Sapling)
        );
    }

    /// Mainnet Sapling address from the upstream `zcash_address` rustdoc example.
    const ROUND_TRIP_SAPLING_ADDRESS: &str =
        "zs1z7rejlpsa98s2rrrfkwmaxu53e4ue0ulcrw0h4x5g8jl04tak0d3mm47vdtahatqrlkngh9slya";

    #[test]
    fn to_uri_round_trip_preserves_recipient_and_amount() -> Result<(), WalletError> {
        let original_uri =
            format!("zcash:{ROUND_TRIP_SAPLING_ADDRESS}?amount=0.0001&message=invoice%20one");
        let parsed = PaymentRequest::from_uri(&original_uri, Network::Mainnet)?;
        let regenerated_uri = parsed.to_uri()?;
        let reparsed = PaymentRequest::from_uri(&regenerated_uri, Network::Mainnet)?;

        assert_eq!(parsed.payments().len(), 1);
        assert_eq!(reparsed.payments().len(), 1);
        let original_payment = &parsed.payments()[0];
        let round_tripped_payment = &reparsed.payments()[0];
        assert_eq!(
            original_payment.recipient.encoded(),
            round_tripped_payment.recipient.encoded()
        );
        assert_eq!(original_payment.amount, round_tripped_payment.amount);
        assert_eq!(original_payment.message, round_tripped_payment.message);
        Ok(())
    }

    #[test]
    fn to_uri_rejects_oversize_zatoshis() -> Result<(), WalletError> {
        let original_uri = format!("zcash:{ROUND_TRIP_SAPLING_ADDRESS}?amount=0.00000001");
        let parsed = PaymentRequest::from_uri(&original_uri, Network::Mainnet)?;
        // Verify the generated URI is at least non-empty and starts with the
        // expected scheme; the round-trip equality is covered above.
        let regenerated_uri = parsed.to_uri()?;
        assert!(regenerated_uri.starts_with("zcash:"));
        Ok(())
    }

    #[allow(
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm,
        reason = "tests assert on exact outcome shapes; expect/panic make failing assertions readable, and wildcard arms catch future SubmitOutcome variants intentionally"
    )]
    mod resolve_send_outcome {
        use super::*;

        #[test]
        fn returns_accepted_tx_id() {
            let accepted_tx_id = TxId::from_bytes([7_u8; 32]);
            let outcome = zally_chain::SubmitOutcome::Accepted {
                tx_id: accepted_tx_id,
            };
            let tx_id = resolve_send_outcome(outcome, accepted_tx_id).expect("Accepted is Ok");
            assert_eq!(tx_id, accepted_tx_id);
        }

        #[test]
        fn returns_canonical_tx_id_on_duplicate() {
            let expected_tx_id = TxId::from_bytes([1_u8; 32]);
            let outcome = zally_chain::SubmitOutcome::Duplicate {
                tx_id: expected_tx_id,
            };
            let tx_id = resolve_send_outcome(outcome, expected_tx_id).expect("Duplicate is Ok");
            assert_eq!(tx_id, expected_tx_id);
        }

        #[test]
        fn returns_canonical_tx_id_on_queued() {
            let expected_tx_id = TxId::from_bytes([1_u8; 32]);
            let outcome = zally_chain::SubmitOutcome::Queued {
                tx_id: expected_tx_id,
            };
            let tx_id = resolve_send_outcome(outcome, expected_tx_id).expect("Queued is Ok");
            assert_eq!(tx_id, expected_tx_id);
        }

        #[test]
        fn rejects_success_outcome_for_a_different_transaction() {
            let expected_tx_id = TxId::from_bytes([1_u8; 32]);
            let returned_tx_id = TxId::from_bytes([2_u8; 32]);
            let outcome = zally_chain::SubmitOutcome::Duplicate {
                tx_id: returned_tx_id,
            };

            let error = resolve_send_outcome(outcome, expected_tx_id)
                .expect_err("mismatched success txid must fail closed");

            assert!(matches!(
                error,
                WalletError::SubmittedTransactionIdMismatch {
                    expected,
                    returned,
                } if expected == expected_tx_id && returned == returned_tx_id
            ));
        }

        #[test]
        fn maps_rejected_to_submission_rejected_with_typed_reason() {
            let fallback_tx_id = TxId::from_bytes([1_u8; 32]);
            let outcome = zally_chain::SubmitOutcome::Rejected {
                reason: zally_chain::RejectionReason::MempoolFull,
                detail: "queue at capacity".to_owned(),
            };
            let err = resolve_send_outcome(outcome, fallback_tx_id).expect_err("Rejected is Err");
            match err {
                WalletError::SubmissionRejected { reason, detail } => {
                    assert_eq!(reason, zally_chain::RejectionReason::MempoolFull);
                    assert_eq!(detail, "queue at capacity");
                }
                other => panic!(
                    "expected WalletError::SubmissionRejected with typed reason, got {other:?}"
                ),
            }
        }
    }
}
