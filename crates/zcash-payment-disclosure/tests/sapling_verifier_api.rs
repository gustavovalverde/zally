//! Public interface checks for Sapling disclosure verification.

use sapling::{PaymentAddress, circuit::PreparedSpendVerifyingKey};
use zcash_payment_disclosure::{
    PaymentDisclosure, PaymentDisclosureEvidence, PaymentDisclosureVerificationError,
    verify_disclosure,
};
use zcash_protocol::consensus::{BlockHeight, Parameters};

fn verify_public_contract<ParamsT: Parameters>(
    disclosure: &PaymentDisclosure,
    transaction_bytes: &[u8],
    mined_height: BlockHeight,
    params: &ParamsT,
    spend_verifying_key: &PreparedSpendVerifyingKey,
) -> Result<PaymentDisclosureEvidence, PaymentDisclosureVerificationError> {
    verify_disclosure(
        disclosure,
        transaction_bytes,
        mined_height,
        params,
        spend_verifying_key,
    )
}

fn inspect_public_evidence(evidence: &PaymentDisclosureEvidence) {
    std::hint::black_box(evidence.transaction_id());
    for spend in evidence.sapling_spends() {
        std::hint::black_box(spend.index());
    }
    for output in evidence.sapling_outputs() {
        std::hint::black_box(output.index());
        std::hint::black_box::<PaymentAddress>(output.recipient());
        std::hint::black_box(output.amount_zat());
        std::hint::black_box(output.memo());
    }
}

#[test]
fn verifier_and_evidence_are_publicly_callable() {
    std::hint::black_box(verify_public_contract::<zcash_protocol::consensus::Network>);
    std::hint::black_box(inspect_public_evidence);
}

#[test]
fn verification_errors_preserve_selected_index_context() {
    let error = PaymentDisclosureVerificationError::SpendIndexOutOfBounds {
        index: 3,
        spend_count: 2,
    };
    assert_eq!(
        error.to_string(),
        "Sapling spend index 3 is absent from a bundle with 2 spends"
    );
}
