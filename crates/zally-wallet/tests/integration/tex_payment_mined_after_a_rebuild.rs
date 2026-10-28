//! A ZIP 320 TEX payment that both steps mine in one block keeps its idempotency key after a
//! birthday rebuild.
//!
//! The paying step spends an output of the funding step. A rescan from the birthday recognizes
//! that spend only if it already knows the output, which it learns from the same block, so
//! without help it reads the mined payment as never mined, judges it unmineable once it
//! expires, frees its key, and a retry pays again. The rebuilt wallet knows every ephemeral
//! address the prior wallet used and stores the carried steps before the rescan, so its own
//! scan recognizes the payment. The chain source is only a second guard: a source that reports
//! a mined payment missing cannot free its key.

use zally_chain::Submitter;
use zally_core::{
    BlockHeight, CompactChainMetadata, CompactSaplingOutput, CompactSaplingSpend,
    CompactTransaction, CompactTransparentInput, CompactTransparentOutput, IdempotencyKey,
    PaymentRecipient, TransparentGapLimit, TxId, Zatoshis,
};
use zally_storage::WalletStorage as _;
use zally_testkit::{MockCommitmentTrees, MockSubmitter, MockTransactionStatusMode};
use zally_wallet::{RetryPolicy, SendOutcome, SendPaymentPlan, WalletError, WalletEvent};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BranchId;
use zcash_transparent::address::Script;
use zcash_transparent::keys::NonHardenedChildIndex;

use super::reorg_fixtures::{
    EventLog, FixtureError, FundedChain, funded_chain_with_gap_limit, sync_to_tip,
};

const PAYMENT_KEY: &str = "tex-payout-0001";
const PAYMENT_RECIPIENT_HASH: [u8; 20] = [0x5e; 20];
const PRE_FUNDING_TIP_HEIGHT: u32 = 110;
const SPEND_TIP_HEIGHT: u32 = PRE_FUNDING_TIP_HEIGHT + 20;
/// Blocks a test advances past each block it mines before the wallet builds again.
const BLOCKS_AFTER_EACH_MINED_BLOCK: u32 = 15;
/// Blocks past the payment's mined height where the verdict judges it, beyond its expiry.
const BLOCKS_PAST_EXPIRY: u32 = 200;
/// An ephemeral gap of one: the rebuilt account knows only the first ephemeral address until
/// its rescan meets a use of it.
const EPHEMERAL_GAP_OF_ONE: TransparentGapLimit = TransparentGapLimit {
    external: 20,
    internal: 10,
    ephemeral: 1,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tex_payment_mined_in_one_block_keeps_its_key_after_a_rebuild() -> Result<(), FixtureError>
{
    let paid = TexWallet::new(TransparentGapLimit::DEFAULT)
        .await?
        .pay_then_rebuild()
        .await?;
    paid.mine_both_steps(PayingStepForm::Complete)?;
    let events = paid.settle_past_expiry().await?;

    paid.assert_paid_once(&events).await?;
    paid.assert_recognized_by_the_scan(&events);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mined_payment_the_scan_cannot_recognize_keeps_its_key() -> Result<(), FixtureError> {
    let paid = TexWallet::new(TransparentGapLimit::DEFAULT)
        .await?
        .pay_then_rebuild()
        .await?;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payment_past_the_rebuilt_ephemeral_gap_keeps_its_key_when_the_source_reports_it_missing()
-> Result<(), FixtureError> {
    let mut wallet = TexWallet::new(EPHEMERAL_GAP_OF_ONE).await?;
    wallet
        .pay_and_mine("tex-payout-earlier", [0x40; 20])
        .await?;
    let paid = wallet.pay_then_rebuild().await?;
    paid.assert_funding_step_pays_ephemeral_index(1)?;
    paid.mine_both_steps(PayingStepForm::Complete)?;
    paid.report_every_transaction_missing();
    let events = paid.settle_past_expiry().await?;

    paid.assert_paid_once(&events).await?;
    paid.assert_recognized_by_the_scan(&events);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payment_far_past_the_default_ephemeral_gap_keeps_its_key_when_the_source_reports_it_missing()
-> Result<(), FixtureError> {
    let gap = TransparentGapLimit::DEFAULT.ephemeral;
    let mut wallet = TexWallet::new(TransparentGapLimit::DEFAULT).await?;
    for last_index_of_gap in [gap - 1, 2 * gap - 1, 3 * gap - 1] {
        wallet.receive_at_ephemeral_index(last_index_of_gap).await?;
    }
    let paid = wallet.pay_then_rebuild().await?;
    paid.assert_funding_step_pays_ephemeral_index(3 * gap)?;
    paid.mine_both_steps(PayingStepForm::Complete)?;
    paid.report_every_transaction_missing();
    let events = paid.settle_past_expiry().await?;

    paid.assert_paid_once(&events).await?;
    paid.assert_recognized_by_the_scan(&events);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payment_recognized_after_a_rebuild_is_recognized_after_another()
-> Result<(), FixtureError> {
    let paid = TexWallet::new(TransparentGapLimit::DEFAULT)
        .await?
        .pay_then_rebuild()
        .await?;
    paid.mine_both_steps(PayingStepForm::Complete)?;
    let first = paid.settle_past_expiry().await?;
    paid.assert_recognized_by_the_scan(&first);
    let base = &paid.wallet.funded.base;
    // A sync driver's reconciliation drops each step from the reverted-broadcast ledger once
    // the scan sees it mined; the wallet database then holds the only copy of their bytes.
    for step in [&paid.funding_step, &paid.paying_step] {
        base.storage
            .clear_reverted_broadcast(TxId::from_bytes(*step.txid().as_ref()))
            .await?;
    }

    base.wallet.reset_to_birthday(base.chain.as_ref()).await?;
    paid.report_every_transaction_missing();
    let second = paid.settle_past_expiry().await?;

    paid.assert_paid_once(&second).await?;
    paid.assert_recognized_by_the_scan(&second);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payment_the_scan_passed_without_its_funding_output_is_never_judged_unmineable()
-> Result<(), FixtureError> {
    let paid = TexWallet::new(TransparentGapLimit::DEFAULT)
        .await?
        .pay_then_rebuild()
        .await?;
    let base = &paid.wallet.funded.base;
    // The rebuilt ledger loses the funding step's bytes, so the rescan cannot know its output
    // before it meets the block that mines both steps.
    base.storage
        .clear_reverted_broadcast(TxId::from_bytes(*paid.funding_step.txid().as_ref()))
        .await?;
    paid.mine_both_steps(PayingStepForm::Complete)?;
    paid.report_every_transaction_missing();
    let events = paid.settle_past_expiry().await?;
    // By now the scan has recorded the funding output; a later verdict must still wait.
    base.wallet.sync(base.chain.as_ref()).await?;

    assert!(
        events.snapshot().iter().any(|event| matches!(
            event,
            WalletEvent::PendingBroadcastVerdictWithheld { tx_id, .. }
                if *tx_id == paid.paying_tx_id
        )),
        "the wallet reports that it holds the verdict"
    );
    assert!(
        !events
            .snapshot()
            .iter()
            .any(|event| matches!(event, WalletEvent::PendingBroadcastUnmineable { .. })),
        "a payment the scan may have missed is never judged unmineable"
    );
    assert_eq!(
        base.storage
            .find_idempotent_submission(&idempotency_key(PAYMENT_KEY)?)
            .await?,
        Some(paid.paying_tx_id),
        "the key stays bound to the payment"
    );
    let retry_submitter =
        MockSubmitter::accepting(base.network).with_tx_id_derivation(nu7_transaction_id);
    let retried = pay_tex(
        &paid.wallet.funded,
        PAYMENT_KEY,
        PAYMENT_RECIPIENT_HASH,
        &retry_submitter,
    )
    .await;
    assert!(
        matches!(
            retried,
            Err(WalletError::PendingBroadcastUnsettled { tx_id, .. }) if tx_id == paid.paying_tx_id
        ),
        "a retry waits for the verdict: {retried:?}"
    );
    assert!(
        retry_submitter.handle().submitted_bytes().is_empty(),
        "a retry pays nothing again"
    );
    Ok(())
}

/// How the mock serves the paying step to the wallet's scan.
#[derive(Clone, Copy)]
enum PayingStepForm {
    /// With its transparent inputs, as a source serves it.
    Complete,
    /// Without its transparent inputs, so no scan can match its spend.
    WithoutTransparentInputs,
}

/// A funded wallet synced to [`SPEND_TIP_HEIGHT`] that pays TEX addresses.
struct TexWallet {
    funded: FundedChain,
    /// Mined transactions with Sapling outputs, in chain order, for the mock's trees.
    sapling_history: Vec<Transaction>,
    tip_height: u32,
}

impl TexWallet {
    async fn new(gap_limit: TransparentGapLimit) -> Result<Self, FixtureError> {
        let funded = funded_chain_with_gap_limit(PRE_FUNDING_TIP_HEIGHT, gap_limit).await?;
        let base = &funded.base;
        base.chain_handle
            .advance_tip(BlockHeight::from(SPEND_TIP_HEIGHT));
        sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
        base.wallet.set_retry_policy(RetryPolicy::none());
        let sapling_history = vec![funded.shielding.clone()];
        Ok(Self {
            funded,
            sapling_history,
            tip_height: SPEND_TIP_HEIGHT,
        })
    }

    /// Pays a TEX address under `key`, mines both steps in the next block, and syncs past it,
    /// so the payment uses up an ephemeral address.
    async fn pay_and_mine(
        &mut self,
        key: &str,
        recipient_hash: [u8; 20],
    ) -> Result<(), FixtureError> {
        let (funding_step, paying_step) = self.pay(key, recipient_hash).await?;
        let block = vec![
            compact_transaction(&funding_step, 1)?,
            compact_transaction(&paying_step, 2)?,
        ];
        self.sapling_history.push(funding_step);
        self.mine_next_block_and_sync(block).await
    }

    /// Mines a payment from outside the wallet to its ephemeral address at `index`, which
    /// moves the start of its ephemeral gap past `index`.
    async fn receive_at_ephemeral_index(&mut self, index: u32) -> Result<(), FixtureError> {
        let index_byte = u8::try_from(index).map_err(|_| FixtureError::Fixture("index"))?;
        let receipt = CompactTransaction {
            index: 1,
            tx_id: TxId::from_bytes([index_byte; 32]),
            fee_zat: None,
            sapling_spends: Vec::new(),
            sapling_outputs: Vec::new(),
            orchard_actions: Vec::new(),
            ironwood_actions: Vec::new(),
            transparent_inputs: Vec::new(),
            transparent_outputs: vec![CompactTransparentOutput {
                value_zat: Zatoshis::try_from(10_000_u64)
                    .map_err(|_| FixtureError::Fixture("receipt value"))?,
                script_pub_key_bytes: ephemeral_script(&self.funded, index)?,
            }],
        };
        self.mine_next_block_and_sync(vec![receipt]).await
    }

    async fn mine_next_block_and_sync(
        &mut self,
        block: Vec<CompactTransaction>,
    ) -> Result<(), FixtureError> {
        let base = &self.funded.base;
        let mined_height = self.tip_height + 1;
        base.chain_handle.mine_transactions(
            BlockHeight::from(mined_height),
            block,
            sapling_trees_after(&self.sapling_history.iter().collect::<Vec<_>>())?,
        );
        self.tip_height = mined_height + BLOCKS_AFTER_EACH_MINED_BLOCK;
        base.chain_handle
            .advance_tip(BlockHeight::from(self.tip_height));
        sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
        Ok(())
    }

    /// Pays a TEX address under `key` and returns its funding and paying steps.
    async fn pay(
        &self,
        key: &str,
        recipient_hash: [u8; 20],
    ) -> Result<(Transaction, Transaction), FixtureError> {
        let submitter = MockSubmitter::accepting(self.funded.base.network)
            .with_tx_id_derivation(nu7_transaction_id);
        let paid = pay_tex(&self.funded, key, recipient_hash, &submitter).await?;
        let sent_steps = submitter.handle().submitted_bytes();
        let [funding_bytes, paying_bytes] = sent_steps.as_slice() else {
            return Err(FixtureError::Fixture("a TEX payment submits two steps"));
        };
        let funding_step = Transaction::read(funding_bytes.as_slice(), BranchId::Nu7)?;
        let paying_step = Transaction::read(paying_bytes.as_slice(), BranchId::Nu7)?;
        if paid.tx_id() != TxId::from_bytes(*paying_step.txid().as_ref()) {
            return Err(FixtureError::Fixture("the send returns its paying step"));
        }
        Ok((funding_step, paying_step))
    }

    /// Pays a TEX address under [`PAYMENT_KEY`], then rebuilds the wallet from its birthday.
    async fn pay_then_rebuild(self) -> Result<PaidThenRebuilt, FixtureError> {
        let (funding_step, paying_step) = self.pay(PAYMENT_KEY, PAYMENT_RECIPIENT_HASH).await?;
        let base = &self.funded.base;
        base.wallet.reset_to_birthday(base.chain.as_ref()).await?;
        Ok(PaidThenRebuilt {
            paying_tx_id: TxId::from_bytes(*paying_step.txid().as_ref()),
            mined_height: self.tip_height + 1,
            funding_step,
            paying_step,
            wallet: self,
        })
    }
}

/// A wallet that paid a TEX address in two accepted steps, then rebuilt from its birthday.
struct PaidThenRebuilt {
    wallet: TexWallet,
    funding_step: Transaction,
    paying_step: Transaction,
    paying_tx_id: TxId,
    mined_height: u32,
}

impl PaidThenRebuilt {
    fn mine_both_steps(&self, paying_form: PayingStepForm) -> Result<(), FixtureError> {
        let mut paying = compact_transaction(&self.paying_step, 2)?;
        if matches!(paying_form, PayingStepForm::WithoutTransparentInputs) {
            paying.transparent_inputs.clear();
        }
        let mut sapling_history = self.wallet.sapling_history.iter().collect::<Vec<_>>();
        sapling_history.push(&self.funding_step);
        self.wallet.funded.base.chain_handle.mine_transactions(
            BlockHeight::from(self.mined_height),
            vec![compact_transaction(&self.funding_step, 1)?, paying],
            sapling_trees_after(&sapling_history)?,
        );
        Ok(())
    }

    /// Makes the chain source answer every transaction lookup with `NotFound`, so the wallet's
    /// own scan is the only evidence that the payment mined.
    fn report_every_transaction_missing(&self) {
        self.wallet
            .funded
            .base
            .chain_handle
            .set_transaction_status_mode(MockTransactionStatusMode::AlwaysNotFound);
    }

    /// Rescans past the payment's expiry with the settled tip there, so the verdict runs.
    async fn settle_past_expiry(&self) -> Result<EventLog, FixtureError> {
        let base = &self.wallet.funded.base;
        let events = EventLog::record(base.wallet.observe());
        base.chain_handle
            .advance_tip(BlockHeight::from(self.mined_height + BLOCKS_PAST_EXPIRY));
        sync_to_tip(&base.wallet, base.chain.as_ref()).await?;
        base.wallet.sync(base.chain.as_ref()).await?;
        Ok(events)
    }

    fn assert_recognized_by_the_scan(&self, events: &EventLog) {
        assert!(
            events.snapshot().iter().any(|event| matches!(
                event,
                WalletEvent::TransactionConfirmed { tx_id, confirmed_at_height }
                    if *tx_id == self.paying_tx_id
                        && *confirmed_at_height == BlockHeight::from(self.mined_height)
            )),
            "the rescan recognizes the paying step mined in the block that funds it"
        );
    }

    /// Checks the test's premise: the funding step pays the ephemeral address at `index`.
    fn assert_funding_step_pays_ephemeral_index(&self, index: u32) -> Result<(), FixtureError> {
        let script = ephemeral_script(&self.wallet.funded, index)?;
        let is_paid = self
            .funding_step
            .transparent_bundle()
            .is_some_and(|bundle| {
                bundle
                    .vout
                    .iter()
                    .any(|output| output.script_pubkey().0.0 == script)
            });
        assert!(is_paid, "the funding step pays ephemeral address {index}");
        Ok(())
    }

    async fn assert_paid_once(&self, events: &EventLog) -> Result<(), FixtureError> {
        let funded = &self.wallet.funded;
        assert!(
            !events
                .snapshot()
                .iter()
                .any(|event| matches!(event, WalletEvent::PendingBroadcastUnmineable { .. })),
            "a mined payment is never judged unmineable"
        );
        assert_eq!(
            funded
                .base
                .storage
                .find_idempotent_submission(&idempotency_key(PAYMENT_KEY)?)
                .await?,
            Some(self.paying_tx_id),
            "the key stays bound to the mined payment"
        );
        let retry_submitter =
            MockSubmitter::accepting(funded.base.network).with_tx_id_derivation(nu7_transaction_id);
        assert_eq!(
            pay_tex(
                funded,
                PAYMENT_KEY,
                PAYMENT_RECIPIENT_HASH,
                &retry_submitter
            )
            .await?
            .tx_id(),
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

fn idempotency_key(key: &str) -> Result<IdempotencyKey, FixtureError> {
    IdempotencyKey::try_from(key).map_err(|_| FixtureError::Fixture("idempotency key"))
}

/// Locking script of the wallet's ephemeral (ZIP 320) address at `index`.
fn ephemeral_script(funded: &FundedChain, index: u32) -> Result<Vec<u8>, FixtureError> {
    let ufvk = zally_keys::derive_ufvk(
        funded.base.network,
        &funded.base.seed,
        zip32::AccountId::ZERO,
    )
    .map_err(|_| FixtureError::Fixture("viewing key"))?;
    let address_index =
        NonHardenedChildIndex::from_index(index).ok_or(FixtureError::Fixture("address index"))?;
    let address = ufvk
        .p2pkh()
        .ok_or(FixtureError::Fixture("transparent viewing key"))?
        .derive_ephemeral_ivk()
        .and_then(|ivk| ivk.derive_ephemeral_address(address_index))
        .map_err(|_| FixtureError::Fixture("ephemeral address"))?;
    Ok(Script::from(address.script()).0.0)
}

async fn pay_tex(
    funded: &FundedChain,
    key: &str,
    recipient_hash: [u8; 20],
    submitter: &dyn Submitter,
) -> Result<SendOutcome, WalletError> {
    let base = &funded.base;
    let encoded =
        zcash_keys::address::Address::Tex(recipient_hash).encode(&base.network.to_parameters());
    let invalid = |field: &str| WalletError::PaymentRequestParseFailed {
        reason: field.to_owned(),
    };
    base.wallet
        .send_payment(SendPaymentPlan::conventional(
            base.account_id,
            IdempotencyKey::try_from(key).map_err(|_| invalid("idempotency key"))?,
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
