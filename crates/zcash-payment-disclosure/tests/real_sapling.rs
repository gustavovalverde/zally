//! Local real-cryptography round trip for Draft1 Sapling disclosures.

use std::convert::Infallible;

use incrementalmerkletree::{frontier::CommitmentTree, witness::IncrementalWitness};
use rand::{SeedableRng, rngs::StdRng};
use sapling::{
    Node, Rseed, note_encryption::prf_ock, value::NoteValue, zip32::ExtendedSpendingKey,
};
use zcash_payment_disclosure::{
    PaymentDisclosurePlan, SaplingOutputSelection, SaplingSpendProvingInput, prove_disclosure,
    verify_disclosure,
};
use zcash_primitives::transaction::{
    builder::{BuildConfig, Builder},
    fees::zip317,
};
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::{
    consensus::{NetworkUpgrade, Parameters, TEST_NETWORK},
    memo::MemoBytes,
    value::Zatoshis,
};
use zcash_transparent::builder::TransparentSigningSet;

const INPUT_AMOUNT_ZAT: u64 = 100_000;
const OUTPUT_AMOUNT_ZAT: u64 = 90_000;

#[test]
#[ignore = "requires Sapling proving parameters in the platform default location"]
#[allow(
    clippy::too_many_lines,
    reason = "the single end-to-end test keeps the note, transaction, disclosure, and evidence chain visible"
)]
fn real_sapling_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let prover = LocalTxProver::with_default_location().ok_or("Sapling parameters unavailable")?;
    let (spend_verifying_key, _) = prover.verifying_keys();
    let prepared_spend_verifying_key = spend_verifying_key.prepare();

    let spending_key = ExtendedSpendingKey::master(&[7; 32]);
    let full_viewing_key = spending_key.to_diversifiable_full_viewing_key();
    let recipient = full_viewing_key.default_address().1;
    let input_rseed = Rseed::AfterZip212([9; 32]);
    let input_note = recipient.create_note(NoteValue::from_raw(INPUT_AMOUNT_ZAT), input_rseed);

    let mut tree = CommitmentTree::<Node, 32>::empty();
    tree.append(Node::from_cmu(&input_note.cmu()))
        .map_err(|()| "synthetic tree is full")?;
    let witness = IncrementalWitness::from_tree(tree).ok_or("synthetic tree has no witness")?;
    let witness_root = witness.root();
    let merkle_path = witness.path().ok_or("synthetic witness has no path")?;

    let target_height = TEST_NETWORK
        .activation_height(NetworkUpgrade::Nu5)
        .ok_or("NU5 activation height missing")?;
    let mut builder = Builder::new(
        TEST_NETWORK,
        target_height,
        BuildConfig::Standard {
            sapling_anchor: Some(witness_root.into()),
            orchard_anchor: None,
            ironwood_anchor: None,
            orchard_pool_bundle_type: orchard::builder::BundleType::DEFAULT,
        },
    );
    builder.add_sapling_spend::<Infallible>(
        full_viewing_key.fvk().clone(),
        input_note,
        merkle_path.clone(),
    )?;
    builder.add_sapling_output::<Infallible>(
        Some(spending_key.expsk.ovk),
        recipient,
        Zatoshis::const_from_u64(OUTPUT_AMOUNT_ZAT),
        MemoBytes::empty(),
    )?;
    let built = builder.build(
        &TransparentSigningSet::new(),
        std::slice::from_ref(&spending_key),
        &[],
        StdRng::from_seed([1; 32]),
        &prover,
        &prover,
        &zip317::FeeRule::standard(),
    )?;
    let transaction = built.transaction();
    let sapling_bundle = transaction
        .sapling_bundle()
        .ok_or("synthetic transaction has no Sapling bundle")?;
    let spend_index = built
        .sapling_meta()
        .spend_index(0)
        .ok_or("synthetic spend index missing")?;
    let output_index = built
        .sapling_meta()
        .output_index(0)
        .ok_or("synthetic output index missing")?;
    let on_chain_output = &sapling_bundle.shielded_outputs()[output_index];
    let ock = prf_ock(
        &spending_key.expsk.ovk,
        on_chain_output.cv(),
        &on_chain_output.cmu().to_bytes(),
        on_chain_output.ephemeral_key(),
    );

    let plan = PaymentDisclosurePlan::new(
        transaction.txid(),
        TEST_NETWORK.network_type(),
        b"real-sapling-round-trip".to_vec(),
        vec![SaplingSpendProvingInput::new(
            u32::try_from(spend_index)?,
            recipient,
            INPUT_AMOUNT_ZAT,
            input_rseed,
            spending_key.expsk.proof_generation_key(),
            witness_root.into(),
            merkle_path,
        )],
        vec![SaplingOutputSelection::new(
            u32::try_from(output_index)?,
            ock.0,
        )],
    )?;
    let disclosure = prove_disclosure(
        plan,
        &spending_key.expsk.ask,
        &prover,
        &mut StdRng::from_seed([2; 32]),
    )?;
    let mut transaction_bytes = Vec::new();
    transaction.write(&mut transaction_bytes)?;
    let evidence = verify_disclosure(
        &disclosure,
        &transaction_bytes,
        target_height,
        &TEST_NETWORK,
        &prepared_spend_verifying_key,
    )?;

    assert_eq!(evidence.transaction_id(), transaction.txid());
    assert_eq!(
        evidence.sapling_spends()[0].index(),
        u32::try_from(spend_index)?
    );
    assert_eq!(
        evidence.sapling_outputs()[0].index(),
        u32::try_from(output_index)?
    );
    assert_eq!(evidence.sapling_outputs()[0].recipient(), recipient);
    assert_eq!(
        evidence.sapling_outputs()[0].amount_zat(),
        OUTPUT_AMOUNT_ZAT
    );
    let mut empty_memo = [0; 512];
    empty_memo[0] = 0xf6;
    assert_eq!(evidence.sapling_outputs()[0].memo(), &empty_memo);
    Ok(())
}
