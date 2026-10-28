//! A ZIP 320 TEX payment that both steps mine in one block keeps its idempotency key after a
//! birthday rebuild.
//!
//! The paying step spends an output of the funding step. A rescan from the birthday recognizes
//! that spend only if it already knows the output, which it learns from the same block, so
//! without help it reads the mined payment as never mined, judges it unmineable once it
//! expires, frees its key, and a retry pays again. The wallet stores the carried steps before
//! the rescan, and asks the chain source before any unmineable verdict frees a key.

use zally_chain::Submitter;
use zally_core::{
    BlockHeight, CompactChainMetadata, CompactSaplingOutput, CompactSaplingSpend,
    CompactTransaction, CompactTransparentInput, CompactTransparentOutput, IdempotencyKey,
    PaymentRecipient, TxId, Zatoshis,
};
use zally_storage::WalletStorage as _;
use zally_testkit::{MockCommitmentTrees, MockSubmitter};
use zally_wallet::{RetryPolicy, SendOutcome, SendPaymentPlan, WalletError, WalletEvent};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BranchId;

use super::reorg_fixtures::{EventLog, FixtureError, FundedChain, funded_chain, sync_to_tip};

const PAYMENT_KEY: &str = "tex-payout-0001";
const PRE_FUNDING_TIP_HEIGHT: u32 = 110;
const SPEND_TIP_HEIGHT: u32 = PRE_FUNDING_TIP_HEIGHT + 20;
const MINED_HEIGHT: u32 = SPEND_TIP_HEIGHT + 1;
/// Far past the payment's expiry, so the verdict would judge an unrecognized payment.
const SETTLED_PAST_EXPIRY_HEIGHT: u32 = SPEND_TIP_HEIGHT + 200;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tex_payment_mined_in_one_block_keeps_its_key_after_a_rebuild() -> Result<(), FixtureError>
{
    let paid = paid_then_rebuilt().await?;
    paid.mine_both_steps(PayingStepForm::Complete)?;
    let events = paid.settle_past_expiry().await?;

    assert!(
        events.snapshot().iter().any(|event| matches!(
            event,
            WalletEvent::TransactionConfirmed { tx_id, confirmed_at_height }
                if *tx_id == paid.paying_tx_id
                    && *confirmed_at_height == BlockHeight::from(MINED_HEIGHT)
        )),
        "the rescan recognizes the paying step mined in the block that funds it"
    );
    paid.assert_paid_once(&events).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mined_payment_the_scan_cannot_recognize_keeps_its_key() -> Result<(), FixtureError> {
    let paid = paid_then_rebuilt().await?;
    paid.mine_both_steps(PayingStepForm::WithoutTransparentInputs)?;
    let events = paid.settle_past_expiry().await?;

    assert!(
        !events.snapshot().iter().any(|event| matches!(
            event,
            WalletEvent::TransactionConfirmed { tx_id, .. } if *tx_id == paid.paying_tx_id
        )),
        "a compact block without the paying step's inputs leaves the scan blind to it"
    );
    paid.assert_paid_once(&events).await
}

/// How the mock serves the paying step to the wallet's scan.
#[derive(Clone, Copy)]
enum PayingStepForm {
    /// With its transparent inputs, as a source serves it.
    Complete,
    /// Without its transparent inputs, so no scan can match its spend.
    WithoutTransparentInputs,
}

/// A wallet that paid a TEX address in two accepted steps, then rebuilt from its birthday.
struct PaidThenRebuilt {
    funded: FundedChain,
    funding_step: Transaction,
    paying_step: Transaction,
    paying_tx_id: TxId,
}

async fn paid_then_rebuilt() -> Result<PaidThenRebuilt, FixtureError> {
    let funded = funded_chain(PRE_FUNDING_TIP_HEIGHT).await?;
    let base = &funded.base;
    base.chain_handle
        .advance_tip(BlockHeight::from(SPEND_TIP_HEIGHT));
    sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
    base.wallet.set_retry_policy(RetryPolicy::none());
    let submitter =
        MockSubmitter::accepting(base.network).with_tx_id_derivation(nu7_transaction_id);
    let paid = pay_tex(&funded, &submitter).await?;
    let sent_steps = submitter.handle().submitted_bytes();
    let [funding_bytes, paying_bytes] = sent_steps.as_slice() else {
        return Err(FixtureError::Fixture("a TEX payment submits two steps"));
    };
    let funding_step = Transaction::read(funding_bytes.as_slice(), BranchId::Nu7)?;
    let paying_step = Transaction::read(paying_bytes.as_slice(), BranchId::Nu7)?;
    let paying_tx_id = TxId::from_bytes(*paying_step.txid().as_ref());
    if paid.tx_id() != paying_tx_id {
        return Err(FixtureError::Fixture("the send returns its paying step"));
    }
    base.wallet.reset_to_birthday(base.chain.as_ref()).await?;
    Ok(PaidThenRebuilt {
        funded,
        funding_step,
        paying_step,
        paying_tx_id,
    })
}

impl PaidThenRebuilt {
    fn mine_both_steps(&self, paying_form: PayingStepForm) -> Result<(), FixtureError> {
        let mut paying = compact_transaction(&self.paying_step, 2)?;
        if matches!(paying_form, PayingStepForm::WithoutTransparentInputs) {
            paying.transparent_inputs.clear();
        }
        self.funded.base.chain_handle.mine_transactions(
            BlockHeight::from(MINED_HEIGHT),
            vec![compact_transaction(&self.funding_step, 1)?, paying],
            sapling_trees_after(&[&self.funded.shielding, &self.funding_step])?,
        );
        Ok(())
    }

    /// Rescans past the payment's expiry with the settled tip there, so the verdict runs.
    async fn settle_past_expiry(&self) -> Result<EventLog, FixtureError> {
        let base = &self.funded.base;
        let events = EventLog::record(base.wallet.observe());
        base.chain_handle
            .advance_tip(BlockHeight::from(SETTLED_PAST_EXPIRY_HEIGHT));
        sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
        base.wallet.sync(base.chain.as_ref()).await?;
        Ok(events)
    }

    async fn assert_paid_once(&self, events: &EventLog) -> Result<(), FixtureError> {
        let base = &self.funded.base;
        assert!(
            !events
                .snapshot()
                .iter()
                .any(|event| matches!(event, WalletEvent::PendingBroadcastUnmineable { .. })),
            "a mined payment is never judged unmineable"
        );
        assert_eq!(
            base.storage
                .find_idempotent_submission(&payment_key()?)
                .await?,
            Some(self.paying_tx_id),
            "the key stays bound to the mined payment"
        );
        let retry_submitter =
            MockSubmitter::accepting(base.network).with_tx_id_derivation(nu7_transaction_id);
        assert_eq!(
            pay_tex(&self.funded, &retry_submitter).await?.tx_id(),
            self.paying_tx_id,
            "a retry answers with the mined payment"
        );
        assert!(
            retry_submitter.handle().submitted_bytes().is_empty(),
            "a retry pays nothing again"
        );
        Ok(())
    }
}

fn payment_key() -> Result<IdempotencyKey, FixtureError> {
    IdempotencyKey::try_from(PAYMENT_KEY).map_err(|_| FixtureError::Fixture("idempotency key"))
}

async fn pay_tex(
    funded: &FundedChain,
    submitter: &dyn Submitter,
) -> Result<SendOutcome, WalletError> {
    let base = &funded.base;
    let encoded =
        zcash_keys::address::Address::Tex([0x5e; 20]).encode(&base.network.to_parameters());
    let invalid = |field: &str| WalletError::PaymentRequestParseFailed {
        reason: field.to_owned(),
    };
    base.wallet
        .send_payment(SendPaymentPlan::conventional(
            base.account_id,
            IdempotencyKey::try_from(PAYMENT_KEY).map_err(|_| invalid("idempotency key"))?,
            PaymentRecipient::TexAddress {
                encoded,
                network: base.network,
            },
            Zatoshis::try_from(50_000_u64).map_err(|_| invalid("amount"))?,
            submitter,
        ))
        .await
}

fn nu7_transaction_id(raw_tx: &[u8]) -> TxId {
    Transaction::read(raw_tx, BranchId::Nu7).map_or(TxId::from_bytes([0; 32]), |transaction| {
        TxId::from_bytes(*transaction.txid().as_ref())
    })
}

/// Compact form of a transaction that carries Sapling and transparent components only.
fn compact_transaction(
    transaction: &Transaction,
    index: u64,
) -> Result<CompactTransaction, FixtureError> {
    if transaction.orchard_bundle().is_some() || transaction.ironwood_bundle().is_some() {
        return Err(FixtureError::Fixture(
            "the fixture serves Sapling and transparent components only",
        ));
    }
    let (sapling_spends, sapling_outputs) = match transaction.sapling_bundle() {
        Some(bundle) => (
            bundle
                .shielded_spends()
                .iter()
                .map(|spend| CompactSaplingSpend {
                    nullifier_bytes: spend.nullifier().0,
                })
                .collect(),
            bundle
                .shielded_outputs()
                .iter()
                .map(|output| {
                    let ciphertext_bytes: [u8; 52] = output.enc_ciphertext()[..52]
                        .try_into()
                        .map_err(|_| FixtureError::Fixture("Sapling ciphertext is too short"))?;
                    Ok(CompactSaplingOutput {
                        commitment_bytes: output.cmu().to_bytes(),
                        ephemeral_key_bytes: output.ephemeral_key().0,
                        ciphertext_bytes,
                    })
                })
                .collect::<Result<Vec<_>, FixtureError>>()?,
        ),
        None => (Vec::new(), Vec::new()),
    };
    let (transparent_inputs, transparent_outputs) = match transaction.transparent_bundle() {
        Some(bundle) => (
            bundle
                .vin
                .iter()
                .map(|input| CompactTransparentInput {
                    previous_tx_id: TxId::from_bytes(*input.prevout().hash()),
                    previous_output_index: input.prevout().n(),
                })
                .collect(),
            bundle
                .vout
                .iter()
                .map(|output| {
                    Ok(CompactTransparentOutput {
                        value_zat: Zatoshis::try_from(output.value().into_u64())
                            .map_err(|_| FixtureError::Fixture("output value out of range"))?,
                        script_pub_key_bytes: output.script_pubkey().0.0.clone(),
                    })
                })
                .collect::<Result<Vec<_>, FixtureError>>()?,
        ),
        None => (Vec::new(), Vec::new()),
    };
    Ok(CompactTransaction {
        index,
        tx_id: TxId::from_bytes(*transaction.txid().as_ref()),
        fee_zat: None,
        sapling_spends,
        sapling_outputs,
        orchard_actions: Vec::new(),
        ironwood_actions: Vec::new(),
        transparent_inputs,
        transparent_outputs,
    })
}

/// Commitment trees after the Sapling outputs of `transactions`, in order, on a chain whose
/// trees were empty before them.
fn sapling_trees_after(transactions: &[&Transaction]) -> Result<MockCommitmentTrees, FixtureError> {
    let mut tree = incrementalmerkletree::frontier::CommitmentTree::<sapling::Node, 32>::empty();
    let mut sapling_commitment_tree_size = 0_u32;
    for output in transactions
        .iter()
        .filter_map(|transaction| transaction.sapling_bundle())
        .flat_map(sapling::Bundle::shielded_outputs)
    {
        tree.append(sapling::Node::from_cmu(output.cmu()))
            .map_err(|()| FixtureError::Fixture("synthetic Sapling tree is full"))?;
        sapling_commitment_tree_size = sapling_commitment_tree_size.saturating_add(1);
    }
    let mut sapling_frontier_bytes = Vec::new();
    zcash_primitives::merkle_tree::write_commitment_tree(&tree, &mut sapling_frontier_bytes)?;
    Ok(MockCommitmentTrees {
        sapling_frontier_bytes,
        orchard_frontier_bytes: vec![0, 0, 0],
        ironwood_frontier_bytes: vec![0, 0, 0],
        chain_metadata: CompactChainMetadata {
            sapling_commitment_tree_size,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        },
    })
}
