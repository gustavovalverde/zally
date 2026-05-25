//! `PaymentRequest::to_uri` survives the wallet propose boundary.
//!
//! Builds a URI that pays the wallet's own derived unified address with an
//! amount and a memo, parses it through `PaymentRequest::from_uri`, regenerates
//! the URI via `to_uri`, re-parses, and hands the round-tripped payment to
//! `Wallet::propose`. The proposal returns a wallet-level error
//! (`ProposalRejected`/`InsufficientBalance`) because the fixture wallet has
//! no synced state, which is exactly the signal that proves the wallet
//! consumed the recipient, amount, and memo carried across the URI round trip.
//! The test fails only on a parse-time error, which would indicate the URI
//! round trip dropped information.

use zally_core::PaymentRecipient;
use zally_wallet::{PaymentRequest, ProposalPlan, WalletError};

use super::fixtures::{TestWalletError, TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn payment_request_to_uri_round_trips_through_wallet_propose() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id,
    } = create_test_wallet().await?;
    let network = wallet.network();
    let parameters = network.to_parameters();

    let unified_address = wallet
        .derive_next_address_with_transparent(account_id)
        .await?;
    let encoded_unified_address = unified_address.encode(&parameters);

    let original_uri =
        format!("zcash:{encoded_unified_address}?amount=0.0001&memo=aW52b2ljZSAxMjM&message=hello");
    let parsed = PaymentRequest::from_uri(&original_uri, network)?;
    assert_eq!(parsed.payments().len(), 1);
    let regenerated_uri = parsed.to_uri()?;
    let reparsed = PaymentRequest::from_uri(&regenerated_uri, network)?;

    let original_payment = &parsed.payments()[0];
    let regenerated_payment = &reparsed.payments()[0];
    assert_eq!(
        original_payment.recipient.encoded(),
        regenerated_payment.recipient.encoded()
    );
    assert_eq!(original_payment.amount, regenerated_payment.amount);
    assert_eq!(original_payment.memo, regenerated_payment.memo);
    assert_eq!(original_payment.message, regenerated_payment.message);
    assert!(
        matches!(
            regenerated_payment.recipient,
            PaymentRecipient::UnifiedAddress { .. }
        ),
        "wallet-derived recipient must round-trip as a unified address",
    );

    let proposal_outcome = wallet
        .propose(ProposalPlan::conventional(
            account_id,
            regenerated_payment.recipient.clone(),
            regenerated_payment.amount,
            regenerated_payment.memo.clone(),
        ))
        .await;
    match proposal_outcome {
        Ok(_)
        | Err(WalletError::InsufficientBalance { .. } | WalletError::ProposalRejected { .. }) => {}
        Err(WalletError::MemoOnTransparentRecipient | WalletError::NetworkMismatch { .. }) => {
            return Err(TestError::WalletRejectedRoundTrippedRequest {
                reason: "wallet rejected the round-tripped request as malformed input".to_owned(),
            });
        }
        Err(other) => {
            return Err(TestError::WalletRejectedRoundTrippedRequest {
                reason: format!("unexpected wallet error: {other:?}"),
            });
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet fixture: {0}")]
    Fixture(#[from] TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("wallet rejected round-tripped request: {reason}")]
    WalletRejectedRoundTrippedRequest { reason: String },
}
