//! A send binds its idempotency key to the paying transaction before submitting anything.
//!
//! A lost acknowledgement or a node rejection therefore never lets a retry with the same key
//! pay twice, and a send that stopped before its paying transaction reached a submitter
//! frees the key.
//!
//! The single-step sends shield a transparent UTXO through
//! `Wallet::shield_transparent_funds`. The multi-step sends pay a ZIP 320 TEX address through
//! `Wallet::send_payment`: the first step moves the shielded note to an ephemeral transparent
//! address and the second pays the recipient from it.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Notify;
use zally_chain::{
    ChainSourceError, RejectionReason, SubmitOutcome, Submitter, SubmitterError,
    parse_transaction_id,
};
use zally_core::{AccountId, BlockHeight, IdempotencyKey, Network, PaymentRecipient, TxId};
use zally_storage::WalletStorage as _;
use zally_wallet::{
    CircuitBreakerConfig, RetryPolicy, SendOutcome, SendPaymentPlan, ShieldTransparentPlan, Wallet,
    WalletError, WalletOptions,
};

use super::reorg_fixtures::{
    ChainWallet, FixtureError, TRANSPARENT_FUNDING_TX_ID, chain_wallet_with_options, funded_chain,
    record_transparent_utxo, sync_to_tip,
};

const SEND_KEY: &str = "payout-0001";
const PRE_FUNDING_TIP_HEIGHT: u32 = 110;
/// Tip at which the funded note has the confirmations a TEX payment's first step needs.
const SPEND_TIP_HEIGHT: u32 = PRE_FUNDING_TIP_HEIGHT + 20;
const TEX_PAYMENT_ZAT: u64 = 50_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_acknowledgement_never_pays_twice() -> Result<(), FixtureError> {
    let base = shieldable_wallet().await?;
    let submitter = ScriptedSubmitter::new(base.network, vec![NodeResponse::LoseAcknowledgement]);

    let first = shield(&base, &submitter).await;
    assert!(
        matches!(&first, Err(error) if error.is_retryable()),
        "the lost acknowledgement surfaces as a retryable error: {first:?}"
    );
    let retried = shield(&base, &submitter).await?.tx_id();
    assert_eq!(
        submitter.received(),
        vec![retried],
        "the node received one transaction, and the retry returns it"
    );
    assert_eq!(
        pending_inputs_of(&base.wallet, base.account_id).await?,
        vec![retried],
        "the in-flight broadcast keeps its pending inputs"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_broadcasts_a_bound_transaction_no_node_received() -> Result<(), FixtureError> {
    let base = shieldable_wallet().await?;
    let submitter = ScriptedSubmitter::new(base.network, vec![NodeResponse::Unreachable]);

    let first = shield(&base, &submitter).await;
    assert!(
        matches!(&first, Err(error) if error.is_retryable()),
        "the unreachable node surfaces as a retryable error: {first:?}"
    );
    assert!(submitter.received().is_empty());
    let retried = shield(&base, &submitter).await?;
    assert_eq!(
        submitter.received(),
        vec![retried.tx_id()],
        "the retry broadcasts the bound transaction instead of only returning its identifier"
    );
    assert!(
        retried.signed.tx_expiry_height.as_u32() > PRE_FUNDING_TIP_HEIGHT,
        "the retry reports the bound transaction's expiry"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejection_keeps_the_key_bound() -> Result<(), FixtureError> {
    let base = shieldable_wallet().await?;
    let submitter = ScriptedSubmitter::new(
        base.network,
        vec![NodeResponse::Reject(RejectionReason::BadConsensusBranch)],
    );

    let first = shield(&base, &submitter).await;
    assert!(
        matches!(&first, Err(WalletError::SubmissionRejected { .. })),
        "the rejection reaches the caller: {first:?}"
    );
    let rejected = submitter.received();
    assert_eq!(rejected.len(), 1);
    assert_eq!(
        base.storage
            .find_idempotent_submission(&send_key()?)
            .await?,
        rejected.first().copied(),
        "a rejection the node judged at its own tip does not free the key"
    );
    assert_eq!(
        pending_inputs_of(&base.wallet, base.account_id).await?,
        rejected
    );
    assert_eq!(
        Some(shield(&base, &submitter).await?.tx_id()),
        rejected.first().copied(),
        "a retry submits the rejected transaction again instead of building another"
    );
    assert_eq!(submitter.received(), rejected);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_the_open_circuit_breaker_stopped_frees_its_key() -> Result<(), FixtureError> {
    // A cooldown far longer than proving can take keeps the breaker open until the submit.
    let base = shieldable_wallet_with_options(
        WalletOptions::default().with_circuit_breaker_config(CircuitBreakerConfig {
            failure_threshold: 5,
            cooldown: Duration::from_hours(24),
        }),
    )
    .await?;
    base.wallet.set_retry_policy(RetryPolicy::none());
    base.chain_handle
        .fail_current_epoch_next(20, || ChainSourceError::Unavailable {
            reason: "simulated outage".into(),
        });
    for _ in 0..5 {
        let _ = base.wallet.sync(base.chain.as_ref()).await;
    }
    let submitter = ScriptedSubmitter::new(base.network, Vec::new());

    let stopped = shield(&base, &submitter).await;
    assert!(
        matches!(&stopped, Err(WalletError::CircuitBroken { .. })),
        "the open breaker stops the send: {stopped:?}"
    );
    assert!(submitter.received().is_empty());
    assert_eq!(
        base.storage
            .find_idempotent_submission(&send_key()?)
            .await?,
        None,
        "a transaction no submitter received frees its key"
    );
    assert!(
        pending_inputs_of(&base.wallet, base.account_id)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_acknowledgement_of_the_paying_step_never_pays_twice() -> Result<(), FixtureError> {
    let (base, _funding_shielding_tx_id) = wallet_with_spendable_note().await?;
    let submitter = ScriptedSubmitter::new(
        base.network,
        vec![NodeResponse::Accept, NodeResponse::LoseAcknowledgement],
    );

    let first = pay_tex(&base, &submitter).await;
    assert!(
        matches!(&first, Err(error) if error.is_retryable()),
        "the lost acknowledgement surfaces as a retryable error: {first:?}"
    );
    let received = submitter.received();
    assert_eq!(received.len(), 2, "both steps reached the node");
    let retried = pay_tex(&base, &submitter).await?.tx_id();
    assert_eq!(
        Some(retried),
        received.last().copied(),
        "the retry returns the step that pays the recipient"
    );
    assert_eq!(
        submitter.received(),
        received,
        "the retry builds nothing new"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_funding_step_frees_the_key_and_is_never_resubmitted() -> Result<(), FixtureError>
{
    let (base, funding_shielding_tx_id) = wallet_with_spendable_note().await?;
    let submitter = ScriptedSubmitter::new(base.network, vec![NodeResponse::LoseAcknowledgement]);

    let first = pay_tex(&base, &submitter).await;
    assert!(first.is_err(), "the funding step failed: {first:?}");
    assert_eq!(
        submitter.received().len(),
        1,
        "the paying step never reached a submitter"
    );
    assert_eq!(
        base.storage
            .find_idempotent_submission(&send_key()?)
            .await?,
        None,
        "a paying step no submitter received frees the key"
    );
    assert!(
        pending_inputs_of(&base.wallet, base.account_id)
            .await?
            .is_empty(),
        "the paying step's pending input is released"
    );

    base.wallet.reset_to_birthday(base.chain.as_ref()).await?;
    let carried: Vec<TxId> = base
        .storage
        .list_reverted_broadcasts()
        .await?
        .into_iter()
        .map(|reverted| reverted.tx_id)
        .collect();
    assert_eq!(
        carried,
        vec![funding_shielding_tx_id],
        "a rebuild carries only what the ledger names, so the driver never broadcasts the \
         released payment after its key may have paid a replacement"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_after_a_crash_before_the_funding_step_submits_every_step()
-> Result<(), FixtureError> {
    let (base, _funding_shielding_tx_id) = wallet_with_spendable_note().await?;
    let stalling = StallingSubmitter {
        network: base.network,
        entered: Arc::new(Notify::new()),
    };
    let entered = Arc::clone(&stalling.entered);
    tokio::select! {
        _ = pay_tex(&base, &stalling) => {
            return Err(FixtureError::Fixture("a stalled submission returned"));
        }
        () = entered.notified() => {}
    }
    // The send ends here, as a crash would, after binding its key and before any node
    // received the funding step.

    let submitter = ScriptedSubmitter::new(base.network, Vec::new());
    let retried = pay_tex(&base, &submitter).await?.tx_id();
    let received = submitter.received();
    assert_eq!(
        received.len(),
        2,
        "the retry submits the funding step before the paying step: {received:?}"
    );
    assert_eq!(received.last().copied(), Some(retried));
    Ok(())
}

/// A wallet synced past [`PRE_FUNDING_TIP_HEIGHT`] that owns one transparent UTXO and submits
/// without retrying, so every submit error reaches the caller.
async fn shieldable_wallet() -> Result<ChainWallet, FixtureError> {
    shieldable_wallet_with_options(WalletOptions::default()).await
}

/// Same as [`shieldable_wallet`], with caller-supplied [`WalletOptions`].
async fn shieldable_wallet_with_options(
    options: WalletOptions,
) -> Result<ChainWallet, FixtureError> {
    let base = chain_wallet_with_options(PRE_FUNDING_TIP_HEIGHT - 100, options).await?;
    // A scan chunk ends at a checkpoint; the one at `PRE_FUNDING_TIP_HEIGHT - 100` anchors the
    // shielding proposal's 100-confirmation policy.
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    base.chain_handle
        .advance_tip(BlockHeight::from(PRE_FUNDING_TIP_HEIGHT));
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    record_transparent_utxo(&base, TRANSPARENT_FUNDING_TX_ID).await?;
    base.wallet.set_retry_policy(RetryPolicy::none());
    Ok(base)
}

/// A wallet with one mined Sapling note it can spend, submitting without retrying, and the
/// transaction that funded the note.
async fn wallet_with_spendable_note() -> Result<(ChainWallet, TxId), FixtureError> {
    let funded = funded_chain(PRE_FUNDING_TIP_HEIGHT).await?;
    let base = funded.base;
    base.chain_handle
        .advance_tip(BlockHeight::from(SPEND_TIP_HEIGHT));
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    base.wallet.set_retry_policy(RetryPolicy::none());
    Ok((base, funded.shielding_tx_id))
}

fn send_key() -> Result<IdempotencyKey, FixtureError> {
    IdempotencyKey::try_from(SEND_KEY).map_err(|_| FixtureError::Fixture("idempotency key"))
}

async fn shield(base: &ChainWallet, submitter: &dyn Submitter) -> Result<SendOutcome, WalletError> {
    let threshold = zally_core::Zatoshis::try_from(1_000_u64).map_err(|_| {
        WalletError::PaymentRequestParseFailed {
            reason: "threshold".into(),
        }
    })?;
    let key =
        IdempotencyKey::try_from(SEND_KEY).map_err(|_| WalletError::PaymentRequestParseFailed {
            reason: "idempotency key".into(),
        })?;
    base.wallet
        .shield_transparent_funds(ShieldTransparentPlan::new(
            base.account_id,
            key,
            threshold,
            submitter,
        ))
        .await
}

async fn pay_tex(
    base: &ChainWallet,
    submitter: &dyn Submitter,
) -> Result<SendOutcome, WalletError> {
    let encoded =
        zcash_keys::address::Address::Tex([0x5e; 20]).encode(&base.network.to_parameters());
    let amount = zally_core::Zatoshis::try_from(TEX_PAYMENT_ZAT).map_err(|_| {
        WalletError::PaymentRequestParseFailed {
            reason: "amount".into(),
        }
    })?;
    let key =
        IdempotencyKey::try_from(SEND_KEY).map_err(|_| WalletError::PaymentRequestParseFailed {
            reason: "idempotency key".into(),
        })?;
    base.wallet
        .send_payment(SendPaymentPlan::conventional(
            base.account_id,
            key,
            PaymentRecipient::TexAddress {
                encoded,
                network: base.network,
            },
            amount,
            submitter,
        ))
        .await
}

async fn pending_inputs_of(
    wallet: &Wallet,
    account_id: AccountId,
) -> Result<Vec<TxId>, WalletError> {
    let mut broadcasts: Vec<TxId> = wallet
        .get_pending_transparent_inputs(account_id)
        .await?
        .inputs
        .into_iter()
        .map(|input| input.broadcast_tx_id)
        .collect();
    broadcasts.dedup();
    Ok(broadcasts)
}

/// Never answers, and signals when a submission reaches it.
struct StallingSubmitter {
    network: Network,
    entered: Arc<Notify>,
}

#[async_trait::async_trait]
impl Submitter for StallingSubmitter {
    fn network(&self) -> Network {
        self.network
    }

    async fn submit(&self, _raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        self.entered.notify_one();
        std::future::pending().await
    }
}

/// One scripted node response.
#[derive(Clone, Copy)]
enum NodeResponse {
    /// The node accepts the transaction.
    Accept,
    /// The node accepts the transaction but its acknowledgement never arrives.
    LoseAcknowledgement,
    /// The submission never reaches the node.
    Unreachable,
    /// The node rejects the transaction.
    Reject(RejectionReason),
}

/// Answers each submission from a script, then as a node does: accepted the first time it
/// receives a transaction, duplicate after that. Records every distinct transaction the node
/// received, in order.
struct ScriptedSubmitter {
    network: Network,
    script: Mutex<Vec<NodeResponse>>,
    received: Mutex<Vec<TxId>>,
}

impl ScriptedSubmitter {
    fn new(network: Network, script: Vec<NodeResponse>) -> Self {
        Self {
            network,
            script: Mutex::new(script),
            received: Mutex::new(Vec::new()),
        }
    }

    fn received(&self) -> Vec<TxId> {
        self.received.lock().clone()
    }
}

#[async_trait::async_trait]
impl Submitter for ScriptedSubmitter {
    fn network(&self) -> Network {
        self.network
    }

    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        let tx_id =
            parse_transaction_id(raw_tx).map_err(|err| SubmitterError::RequestRejected {
                reason: err.to_string(),
            })?;
        let is_unreachable = {
            let mut script = self.script.lock();
            let is_unreachable = matches!(script.first(), Some(NodeResponse::Unreachable));
            if is_unreachable {
                script.remove(0);
            }
            is_unreachable
        };
        if is_unreachable {
            return Err(SubmitterError::Unavailable {
                reason: "node unreachable".to_owned(),
            });
        }
        let is_duplicate = {
            let mut received = self.received.lock();
            let is_duplicate = received.contains(&tx_id);
            if !is_duplicate {
                received.push(tx_id);
            }
            is_duplicate
        };
        let next = {
            let mut script = self.script.lock();
            (!script.is_empty()).then(|| script.remove(0))
        };
        match next {
            Some(NodeResponse::LoseAcknowledgement) => Err(SubmitterError::Unavailable {
                reason: "acknowledgement lost".to_owned(),
            }),
            Some(NodeResponse::Reject(reason)) => Ok(SubmitOutcome::Rejected {
                reason,
                detail: "scripted rejection".to_owned(),
            }),
            Some(NodeResponse::Accept | NodeResponse::Unreachable) | None if is_duplicate => {
                Ok(SubmitOutcome::Duplicate { tx_id })
            }
            Some(NodeResponse::Accept | NodeResponse::Unreachable) | None => {
                Ok(SubmitOutcome::Accepted { tx_id })
            }
        }
    }
}
