//! Same shape as `create_then_open_round_trip` but uses `InMemorySealing` with shared state
//! to simulate process restart without touching the filesystem.

use zally_core::{BlockHeight, Network};
use zally_storage::{Sqlite, SqliteOptions};
use zally_testkit::{InMemorySealing, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn create_then_open_round_trip_in_memory() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();

    let sealing_primary = InMemorySealing::new();
    let sealing_shadow = sealing_primary.shared_with();

    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, account_id, _mnemonic) = Wallet::builder(network, sealing_primary, storage)
        .create(&chain, BlockHeight::from(1))
        .await?;
    let params = network.to_parameters();
    let ua_first = wallet
        .derive_next_address(account_id)
        .await?
        .encode(&params);
    drop(wallet);

    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    let (wallet, account_id_2) = Wallet::builder(network, sealing_shadow, storage)
        .open()
        .await?;
    assert_eq!(account_id, account_id_2);
    let ua_second = wallet
        .derive_next_address(account_id_2)
        .await?
        .encode(&params);
    assert_ne!(ua_first, ua_second);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
