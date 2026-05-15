//! Full round-trip via `AgeFileSealing`: create, derive, drop, re-open (CORE-1, CORE-2, CORE-4).

use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::TempWalletPath;
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn create_then_open_round_trip() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;
    let params = network.to_parameters();
    let ua_first = wallet
        .derive_next_address(account_id)
        .await?
        .encode(&params);
    drop(wallet);

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let (wallet, account_id_2) = Wallet::open(network, sealing, storage).await?;
    assert_eq!(account_id, account_id_2);
    let ua_second = wallet
        .derive_next_address(account_id_2)
        .await?
        .encode(&params);

    assert_ne!(
        ua_first, ua_second,
        "ZIP-316: each derive_next_address call must advance the diversifier index"
    );

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.db_path(),
    ));
    let (wallet, account_id_3) = Wallet::open(network, sealing, storage).await?;
    assert_eq!(account_id, account_id_3);
    let ua_third = wallet
        .derive_next_address(account_id_3)
        .await?
        .encode(&params);
    assert_ne!(
        ua_second, ua_third,
        "diversifier index must continue advancing across opens"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
