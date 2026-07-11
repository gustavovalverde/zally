//! Public codec behavior for immutable payment-disclosure profiles.

use zcash_payment_disclosure::{
    IronwoodOutputDisclosure, IronwoodSpendDisclosure, PaymentDisclosure,
    PaymentDisclosureCodecError, PaymentDisclosureProfile, SaplingOutputDisclosure,
    SaplingSpendDisclosure,
};
use zcash_protocol::{TxId, consensus::NetworkType};

#[test]
fn draft1_signed_bytes_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let disclosure = PaymentDisclosure::new(
        PaymentDisclosureProfile::Zip311Draft1,
        TxId::from_bytes([0x42; 32]),
        b"merchant-challenge".to_vec(),
        vec![SaplingSpendDisclosure::new(
            2,
            [0x11; 32],
            [0x22; 32],
            [0x33; 192],
            [0x44; 64],
        )],
        vec![SaplingOutputDisclosure::new(1, [0x55; 32])],
    )?;

    let signed_bytes = disclosure.to_bytes();
    let decoded = PaymentDisclosure::from_bytes(&signed_bytes)?;

    assert_eq!(decoded, disclosure);
    assert_eq!(decoded.to_bytes(), signed_bytes);
    Ok(())
}

#[test]
fn draft1_constructor_rejects_too_many_outputs() {
    let outputs = (0..4_097)
        .map(|index| SaplingOutputDisclosure::new(index, [0x55; 32]))
        .collect();
    let outcome = PaymentDisclosure::new(
        PaymentDisclosureProfile::Zip311Draft1,
        TxId::from_bytes([0x42; 32]),
        Vec::new(),
        vec![SaplingSpendDisclosure::new(
            0, [0; 32], [0; 32], [0; 192], [0; 64],
        )],
        outputs,
    );

    assert!(matches!(
        outcome,
        Err(PaymentDisclosureCodecError::SizeOutOfRange {
            size: 4_097,
            bound: 4_096,
        })
    ));
}

#[test]
fn draft1_digest_is_bound_to_the_network_coin_type() -> Result<(), Box<dyn std::error::Error>> {
    let disclosure = PaymentDisclosure::new(
        PaymentDisclosureProfile::Zip311Draft1,
        TxId::from_bytes([0x42; 32]),
        b"merchant-challenge".to_vec(),
        vec![SaplingSpendDisclosure::new(
            0,
            [0x11; 32],
            [0x22; 32],
            [0x33; 192],
            [0x44; 64],
        )],
        Vec::new(),
    )?;

    assert_ne!(
        disclosure.compute_digest(NetworkType::Main),
        disclosure.compute_digest(NetworkType::Test),
    );
    Ok(())
}

#[test]
fn draft1_encodes_transaction_id_in_display_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut internal_transaction_id_bytes = [0; 32];
    for (byte, ordinal) in internal_transaction_id_bytes.iter_mut().zip(0_u8..) {
        *byte = ordinal;
    }
    let disclosure = PaymentDisclosure::new(
        PaymentDisclosureProfile::Zip311Draft1,
        TxId::from_bytes(internal_transaction_id_bytes),
        Vec::new(),
        vec![SaplingSpendDisclosure::new(
            0, [0; 32], [0; 32], [0; 192], [0; 64],
        )],
        Vec::new(),
    )?;

    let signed_bytes = disclosure.to_bytes();
    let mut expected_display_bytes = internal_transaction_id_bytes;
    expected_display_bytes.reverse();
    assert_eq!(&signed_bytes[1..33], expected_display_bytes);
    assert_eq!(PaymentDisclosure::from_bytes(&signed_bytes)?, disclosure,);
    Ok(())
}

#[test]
fn zally_ironwood_signed_bytes_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let disclosure = PaymentDisclosure::ironwood_extension(
        TxId::from_bytes([0x24; 32]),
        b"merchant-challenge".to_vec(),
        vec![IronwoodSpendDisclosure::new(1, [0x66; 64])],
        vec![IronwoodOutputDisclosure::new(2, [0x77; 32])],
    )?;

    let signed_bytes = disclosure.to_bytes();
    let decoded = PaymentDisclosure::from_bytes(&signed_bytes)?;

    assert_eq!(decoded.profile(), PaymentDisclosureProfile::ZallyIronwood);
    assert_eq!(decoded, disclosure);
    assert_eq!(decoded.to_bytes(), signed_bytes);
    Ok(())
}
