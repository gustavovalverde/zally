//! `Wallet::propose` refuses memos on transparent recipients per ZIP-302 (SPEND-2).

use zally_core::{Memo, PaymentRecipient, Zatoshis};
use zally_wallet::WalletError;

use super::fixtures::{TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn propose_rejects_memo_on_transparent_recipient() -> Result<(), TestError> {
    let TestWalletFixture {
        temp: _temp,
        wallet,
        account_id: account,
    } = create_test_wallet().await?;
    let network = wallet.network();

    let recipient = PaymentRecipient::TransparentAddress {
        encoded: "t1example".into(),
        network,
    };
    let memo = Memo::from_bytes(b"invoice 42").map_err(TestError::Memo)?;
    let amount = Zatoshis::try_from(100_u64).map_err(TestError::Zat)?;
    let outcome = wallet
        .propose(zally_wallet::ProposalPlan::conventional(
            account,
            recipient,
            amount,
            Some(memo),
        ))
        .await;
    assert!(matches!(
        outcome,
        Err(WalletError::MemoOnTransparentRecipient)
    ));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] super::fixtures::TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("memo error: {0}")]
    Memo(zally_core::MemoError),
    #[error("zat error: {0}")]
    Zat(zally_core::ZatoshisError),
}
