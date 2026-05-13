//! REQ-SPEND-2 — `Wallet::propose` refuses memos on transparent recipients (ZIP-302).

use zally_core::{BlockHeight, Memo, Network, PaymentRecipient, Zatoshis};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn propose_rejects_memo_on_transparent_recipient() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();
    let sealing = InMemorySealing::new();
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let (wallet, account, _) =
        Wallet::create(network, sealing, storage, BlockHeight::from(1)).await?;

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
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("memo error: {0}")]
    Memo(zally_core::MemoError),
    #[error("zat error: {0}")]
    Zat(zally_core::ZatoshisError),
}
