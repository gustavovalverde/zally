//! A send the node only queues is not reported sent until the node reports it held.
//!
//! A node answers a transaction it is still verifying as queued, also when a retried
//! submission arrives while the first one waits for an input no block holds. The node then
//! drops the transaction without another answer, so a queued answer is no evidence of a
//! broadcast. The wallet submits the same bytes again until the node reports the transaction
//! accepted or already known, and otherwise fails the send with
//! `WalletError::BroadcastUnconfirmed` while its key stays bound.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use zally_chain::{ChainSourceError, SubmitOutcome, Submitter, SubmitterError};
use zally_core::{BlockHeight, IdempotencyKey, Network};
use zally_testkit::{MockChainSource, MockSubmitter};
use zally_wallet::{CircuitBreakerConfig, RetryPolicy, WalletError, WalletOptions};

use super::reorg_fixtures::{
    ChainWallet, FixtureError, chain_wallet_with_options, parsed_transaction_id,
    record_transparent_utxo_at, shield_into_sapling, sync_to_tip, transaction_id_submitter,
};

const TIP_HEIGHT: u32 = 300;
const COINBASE_HEIGHT: u32 = 150;
const COINBASE_TX_ID: [u8; 32] = [0x5d; 32];
/// Long enough for at least one submission after the first queued answer.
const CONFIRMATION_BUDGET_MS: u64 = 1_500;

async fn wallet_with_a_mature_coinbase(
    confirmation_budget_ms: u64,
) -> Result<ChainWallet, FixtureError> {
    wallet_with_options(
        WalletOptions::default().with_queued_broadcast_confirmation_ms(confirmation_budget_ms),
    )
    .await
}

async fn wallet_with_options(options: WalletOptions) -> Result<ChainWallet, FixtureError> {
    let base = chain_wallet_with_options(TIP_HEIGHT - 100, options).await?;
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    base.chain_handle.advance_tip(BlockHeight::from(TIP_HEIGHT));
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    record_transparent_utxo_at(&base, COINBASE_TX_ID, COINBASE_HEIGHT).await?;
    Ok(base)
}

fn shielding_key(key: &str) -> Result<IdempotencyKey, FixtureError> {
    IdempotencyKey::try_from(key).map_err(|_| FixtureError::Fixture("idempotency key"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_the_node_keeps_queued_is_unconfirmed_and_keeps_its_key() -> Result<(), FixtureError>
{
    let base = wallet_with_a_mature_coinbase(CONFIRMATION_BUDGET_MS).await?;
    let key = shielding_key("shield-kept-queued")?;
    let queuing = MockSubmitter::queuing(base.network).with_tx_id_derivation(parsed_transaction_id);

    let unconfirmed = shield_into_sapling(&base, key.clone(), &queuing).await;

    let Err(FixtureError::Wallet(WalletError::BroadcastUnconfirmed { tx_id, .. })) = unconfirmed
    else {
        return Err(FixtureError::Fixture(
            "a send the node keeps queued must fail as unconfirmed",
        ));
    };
    let submissions = queuing.handle().submitted_bytes();
    assert!(
        submissions.len() >= 2,
        "a queued answer is followed by another submission of the same bytes, got {}",
        submissions.len()
    );
    assert!(
        submissions
            .iter()
            .all(|raw| parsed_transaction_id(raw) == tx_id),
        "every submission carries the same transaction"
    );

    let accepting = transaction_id_submitter(base.network);
    let outcome = shield_into_sapling(&base, key, &accepting).await?;
    assert_eq!(
        outcome.tx_id(),
        tx_id,
        "the bound key returns the unconfirmed transaction once the node holds it"
    );
    assert_eq!(
        accepting.handle().submitted_bytes(),
        vec![submissions[0].clone()],
        "the retry submits the same bytes instead of building another transaction"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_the_node_accepts_after_queuing_it_is_sent() -> Result<(), FixtureError> {
    let base = wallet_with_a_mature_coinbase(CONFIRMATION_BUDGET_MS).await?;
    let submitter = transaction_id_submitter(base.network);
    submitter.handle().queue_next_submissions(1);

    let outcome =
        shield_into_sapling(&base, shielding_key("shield-queued-once")?, &submitter).await?;

    let submissions = submitter.handle().submitted_bytes();
    assert_eq!(
        submissions.len(),
        2,
        "the queued answer is confirmed by one more submission"
    );
    assert!(
        submissions
            .iter()
            .all(|raw| parsed_transaction_id(raw) == outcome.tx_id())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_budget_fails_on_the_first_queued_answer() -> Result<(), FixtureError> {
    let base = wallet_with_a_mature_coinbase(0).await?;
    let queuing = MockSubmitter::queuing(base.network).with_tx_id_derivation(parsed_transaction_id);

    let unconfirmed =
        shield_into_sapling(&base, shielding_key("shield-without-budget")?, &queuing).await;

    assert!(
        matches!(
            unconfirmed,
            Err(FixtureError::Wallet(
                WalletError::BroadcastUnconfirmed { .. }
            ))
        ),
        "a zero budget fails on the first queued answer, got {unconfirmed:?}"
    );
    assert_eq!(queuing.handle().submission_count(), 1);
    Ok(())
}

/// Regression: a breaker opened while a queued send waits keeps the send bound.
///
/// Another operation can open the circuit breaker during the pause. The next submission then
/// fails before it reaches the node, but the node already holds the transaction, so reading
/// the failure as a send that never reached it would free the key while the node may still
/// mine the transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_breaker_opened_after_a_queued_answer_keeps_the_send_bound() -> Result<(), FixtureError> {
    const BREAKER_COOLDOWN_MS: u64 = 2_000;
    let options = WalletOptions::default()
        .with_queued_broadcast_confirmation_ms(CONFIRMATION_BUDGET_MS)
        .with_circuit_breaker_config(CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(BREAKER_COOLDOWN_MS),
        });
    let base = wallet_with_options(options).await?;
    base.wallet.set_retry_policy(RetryPolicy::none());
    let key = shielding_key("shield-breaker-after-queued")?;
    let submitter = SignallingSubmitter {
        inner: MockSubmitter::queuing(base.network).with_tx_id_derivation(parsed_transaction_id),
        submitted: Notify::new(),
    };
    let failing_chain = MockChainSource::new(base.network);
    failing_chain
        .handle()
        .fail_current_epoch_next(1, || ChainSourceError::Unavailable {
            reason: "simulated outage".into(),
        });

    let (unconfirmed, _) =
        tokio::join!(shield_into_sapling(&base, key.clone(), &submitter), async {
            submitter.submitted.notified().await;
            base.wallet.sync(&failing_chain).await
        });

    let Err(FixtureError::Wallet(WalletError::BroadcastUnconfirmed { tx_id, .. })) = unconfirmed
    else {
        return Err(FixtureError::Fixture(
            "a breaker that opens after a queued answer must leave the send unconfirmed",
        ));
    };
    tokio::time::sleep(Duration::from_millis(BREAKER_COOLDOWN_MS)).await;
    let accepting = transaction_id_submitter(base.network);
    let outcome = shield_into_sapling(&base, key, &accepting).await?;
    assert_eq!(
        outcome.tx_id(),
        tx_id,
        "the key still names the queued transaction"
    );
    Ok(())
}

/// Delegates to a [`MockSubmitter`] and signals after every submission.
struct SignallingSubmitter {
    inner: MockSubmitter,
    submitted: Notify,
}

#[async_trait]
impl Submitter for SignallingSubmitter {
    fn network(&self) -> Network {
        self.inner.network()
    }

    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        let outcome = self.inner.submit(raw_tx).await;
        self.submitted.notify_one();
        outcome
    }
}
