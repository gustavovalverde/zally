//! `Wallet::propose` refuses recipients on a different network.

use zally_core::{BlockHeight, Network, PaymentRecipient, Zatoshis};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{InMemorySealing, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn propose_rejects_network_mismatch() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();
    let sealing = InMemorySealing::new();
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, account, _) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    let recipient = PaymentRecipient::UnifiedAddress {
        encoded: "u1mainnet-example".into(),
        network: Network::Mainnet,
    };
    let amount = Zatoshis::try_from(100_u64).map_err(TestError::Zat)?;
    let outcome = wallet
        .propose(zally_wallet::ProposalPlan::conventional(
            account, recipient, amount, None,
        ))
        .await;
    assert!(matches!(outcome, Err(WalletError::NetworkMismatch { .. })));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("zat error: {0}")]
    Zat(zally_core::ZatoshisError),
}
