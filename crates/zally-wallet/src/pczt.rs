//! PCZT roles bound to a `Wallet`.
//!
//! `Wallet::propose_pczt` builds an unsigned PCZT against the wallet's live notes.
//! `Wallet::sign_pczt` authorises a PCZT with the sealed seed.
//! `Wallet::extract_and_submit_pczt` extracts a finalised PCZT, persists the
//! transaction in the wallet DB, and broadcasts via the supplied `Submitter`.

use zally_chain::Submitter;
use zally_core::{BlockHeight, Network};
use zally_pczt::{PcztBytes, Signer};

use crate::spend::{FeeStrategy, ProposalPlan, SendOutcome};
use crate::wallet::Wallet;
use crate::wallet_error::WalletError;

impl Wallet {
    /// Builds an unsigned PCZT for `plan` against the wallet's live notes.
    ///
    /// Validates the same recipient/memo/fee guards as [`Wallet::propose`], then composes
    /// `propose_standard_transfer_to_address` and `create_pczt_from_proposal` inside the
    /// storage layer. The returned bytes carry the wallet's network and must be authorised
    /// via [`Wallet::sign_pczt`] before submission.
    ///
    /// `not_retryable` on validation failure or insufficient balance; `retryable` on
    /// transient I/O against the wallet database.
    pub async fn propose_pczt(&self, plan: ProposalPlan) -> Result<PcztBytes, WalletError> {
        validate_proposal_plan(&plan, self.network())?;
        let recipient_encoded = plan.recipient.encoded().to_owned();
        let memo_bytes = plan.memo.as_ref().map(memo_to_wire_bytes);
        let raw = self
            .inner
            .storage
            .create_pczt(zally_storage::ProposalPaymentRequest::new(
                plan.account_id,
                recipient_encoded,
                plan.amount_zat.as_u64(),
                memo_bytes,
            ))
            .await
            .map_err(|err| lift_storage_error(&err))?;
        Ok(PcztBytes::from_serialized(raw, self.network()))
    }

    /// Signs `pczt` with the seed unsealed from this wallet's `SeedSealing`.
    ///
    /// Single-key path: derives the account-zero `UnifiedSpendingKey` and applies Sapling,
    /// Orchard, and transparent spend authorizations. Returns
    /// [`zally_pczt::PcztError::NoMatchingKeys`] when the wallet's seed cannot authorize any
    /// spend in the PCZT.
    ///
    /// `not_retryable` on no-matching-keys or upstream signer error; `retryable` on transient
    /// sealing I/O.
    pub async fn sign_pczt(&self, pczt: PcztBytes) -> Result<PcztBytes, WalletError> {
        validate_pczt_network(pczt.network(), self.network())?;
        let seed = self
            .inner
            .sealing
            .unseal_seed()
            .await
            .map_err(WalletError::from)?;
        let pczt_signer = Signer::new(self.network());
        let authorized = pczt_signer.sign_with_seed(pczt, &seed).await?;
        Ok(authorized)
    }

    /// Extracts a finalised PCZT, persists the transaction in the wallet DB, and broadcasts
    /// it through `submitter`.
    ///
    /// The storage layer wraps `extract_and_store_transaction_from_pczt` so the wallet DB
    /// records the spend immediately; the returned `SendOutcome` carries the txid the
    /// submitter accepted (or the wallet's own txid on `Duplicate`).
    ///
    /// `not_retryable` on malformed PCZTs, missing prover params, or submitter rejection;
    /// `retryable` on transient submitter or storage I/O.
    pub async fn extract_and_submit_pczt(
        &self,
        pczt: PcztBytes,
        submitter: &dyn Submitter,
    ) -> Result<SendOutcome, WalletError> {
        validate_pczt_network(pczt.network(), self.network())?;
        if submitter.network() != self.network() {
            return Err(WalletError::NetworkMismatch {
                storage: self.network(),
                requested: submitter.network(),
            });
        }
        let prepared = self
            .inner
            .storage
            .extract_and_store_pczt(pczt.into_bytes())
            .await
            .map_err(|err| lift_storage_error(&err))?;
        let policy = self.retry_policy();
        let outcome = crate::retry::with_breaker_and_retry(
            &self.inner.circuit_breaker,
            policy,
            "extract_and_submit_pczt.submit",
            || submitter.submit(&prepared.raw_bytes),
            |err| map_submitter_error(&err),
        )
        .await?;
        translate_submit_outcome(outcome, prepared.tx_id)
    }
}

fn validate_proposal_plan(plan: &ProposalPlan, wallet_network: Network) -> Result<(), WalletError> {
    if plan.recipient.network() != wallet_network {
        return Err(WalletError::NetworkMismatch {
            storage: wallet_network,
            requested: plan.recipient.network(),
        });
    }
    if let Some(memo) = plan.memo.as_ref()
        && !matches!(memo, zally_core::Memo::Empty)
        && plan.recipient.is_transparent()
    {
        return Err(WalletError::MemoOnTransparentRecipient);
    }
    if plan.amount_zat.as_u64() == 0 {
        return Err(WalletError::ProposalRejected {
            reason: "amount must be positive".into(),
        });
    }
    if !matches!(plan.fee, FeeStrategy::Conventional) {
        return Err(WalletError::ProposalRejected {
            reason: "only ZIP-317 conventional fee is wired in v1".into(),
        });
    }
    Ok(())
}

fn validate_pczt_network(
    pczt_network: Network,
    wallet_network: Network,
) -> Result<(), WalletError> {
    if pczt_network == wallet_network {
        Ok(())
    } else {
        Err(WalletError::NetworkMismatch {
            storage: wallet_network,
            requested: pczt_network,
        })
    }
}

fn lift_storage_error(err: &zally_storage::StorageError) -> WalletError {
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

fn map_submitter_error(err: &zally_chain::SubmitterError) -> WalletError {
    WalletError::ChainSource {
        reason: err.to_string(),
        is_retryable: err.is_retryable(),
    }
}

fn memo_to_wire_bytes(memo: &zally_core::Memo) -> Vec<u8> {
    zally_core::MemoBytes::from(memo).as_slice().to_vec()
}

fn translate_submit_outcome(
    outcome: zally_chain::SubmitOutcome,
    fallback_tx_id: zally_core::TxId,
) -> Result<SendOutcome, WalletError> {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "SubmitOutcome is #[non_exhaustive]; unknown future variants surface as a \
                  non-retryable ProposalRejected so the operator gets a clear failure"
    )]
    match outcome {
        zally_chain::SubmitOutcome::Accepted { tx_id } => Ok(SendOutcome {
            tx_id,
            broadcast_at_height: BlockHeight::from(0),
        }),
        zally_chain::SubmitOutcome::Duplicate { .. } => Ok(SendOutcome {
            tx_id: fallback_tx_id,
            broadcast_at_height: BlockHeight::from(0),
        }),
        zally_chain::SubmitOutcome::Rejected { reason } => {
            Err(WalletError::SubmissionRejected { reason })
        }
        _ => Err(WalletError::ProposalRejected {
            reason: "submitter returned an unrecognised outcome variant".into(),
        }),
    }
}
