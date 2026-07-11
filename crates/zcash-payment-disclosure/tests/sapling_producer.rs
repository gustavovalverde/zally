//! Public behavior of Draft1 Sapling disclosure production.

use incrementalmerkletree::Position;
use rand::{SeedableRng, rngs::StdRng};
use sapling::{MerklePath, Node, Rseed, prover::mock::MockSpendProver, zip32::ExtendedSpendingKey};
use zcash_payment_disclosure::{
    PaymentDisclosure, PaymentDisclosurePlan, PaymentDisclosureProductionError,
    PaymentDisclosureProfile, SaplingOutputSelection, SaplingSpendProvingInput, prove_disclosure,
};
use zcash_protocol::{TxId, consensus::NetworkType};

fn fixture_plan(
    message: &[u8],
    network: NetworkType,
) -> Result<(PaymentDisclosurePlan, sapling::keys::SpendAuthorizingKey), Box<dyn std::error::Error>>
{
    let spending_key = ExtendedSpendingKey::master(&[7; 32]);
    let (_, recipient) = spending_key.default_address();
    let proof_generation_key = spending_key.expsk.proof_generation_key();
    let ask = spending_key.expsk.ask;
    let node = Option::from(Node::from_bytes([0; 32])).ok_or("invalid fixture node")?;
    let witness = MerklePath::from_parts(vec![node; 32], Position::from(0))
        .map_err(|()| "invalid fixture witness")?;
    let plan = PaymentDisclosurePlan::new(
        TxId::from_bytes([0x42; 32]),
        network,
        message.to_vec(),
        vec![SaplingSpendProvingInput::new(
            0,
            recipient,
            50_000,
            Rseed::AfterZip212([9; 32]),
            proof_generation_key,
            sapling::Anchor::from(sapling::Node::from_scalar(bls12_381::Scalar::from(11))),
            witness,
        )],
        vec![SaplingOutputSelection::new(1, [0x55; 32])],
    )?;
    Ok((plan, ask))
}

fn produce_fixture(
    message: &[u8],
    network: NetworkType,
) -> Result<PaymentDisclosure, Box<dyn std::error::Error>> {
    let (plan, ask) = fixture_plan(message, network)?;
    Ok(prove_disclosure(
        plan,
        &ask,
        &MockSpendProver,
        &mut StdRng::from_seed([3; 32]),
    )?)
}

#[test]
fn producer_constructs_a_canonical_draft1_disclosure() -> Result<(), Box<dyn std::error::Error>> {
    let disclosure = produce_fixture(b"merchant-challenge", NetworkType::Test)?;
    assert_eq!(
        PaymentDisclosure::from_bytes(&disclosure.to_bytes())?,
        disclosure
    );
    assert_eq!(disclosure.transaction_id(), TxId::from_bytes([0x42; 32]));
    assert_eq!(disclosure.profile(), PaymentDisclosureProfile::Zip311Draft1);
    assert_eq!(disclosure.message(), b"merchant-challenge");
    Ok(())
}

#[test]
fn message_and_network_change_the_signed_disclosure() -> Result<(), Box<dyn std::error::Error>> {
    let original = produce_fixture(b"challenge-a", NetworkType::Test)?;
    let changed_message = produce_fixture(b"challenge-b", NetworkType::Test)?;
    let changed_network = produce_fixture(b"challenge-a", NetworkType::Main)?;

    assert_ne!(original.to_bytes(), changed_message.to_bytes());
    assert_ne!(original.to_bytes(), changed_network.to_bytes());
    Ok(())
}

#[test]
fn producer_rejects_an_authorizing_key_for_another_account()
-> Result<(), Box<dyn std::error::Error>> {
    let (plan, _) = fixture_plan(b"merchant-challenge", NetworkType::Test)?;
    let unrelated_ask = ExtendedSpendingKey::master(&[8; 32]).expsk.ask;
    let outcome = prove_disclosure(
        plan,
        &unrelated_ask,
        &MockSpendProver,
        &mut StdRng::from_seed([3; 32]),
    );
    assert!(matches!(
        outcome,
        Err(PaymentDisclosureProductionError::SpendAuthorizingKeyMismatch { index: 0 })
    ));
    Ok(())
}

#[test]
fn producer_uses_fresh_randomness_for_each_disclosure() -> Result<(), Box<dyn std::error::Error>> {
    let (first_plan, first_ask) = fixture_plan(b"merchant-challenge", NetworkType::Test)?;
    let (second_plan, second_ask) = fixture_plan(b"merchant-challenge", NetworkType::Test)?;
    let first = prove_disclosure(
        first_plan,
        &first_ask,
        &MockSpendProver,
        &mut StdRng::from_seed([3; 32]),
    )?;
    let second = prove_disclosure(
        second_plan,
        &second_ask,
        &MockSpendProver,
        &mut StdRng::from_seed([4; 32]),
    )?;
    assert_ne!(first.to_bytes(), second.to_bytes());
    Ok(())
}
