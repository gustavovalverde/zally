//! Regression: `Wallet::derive_next_address_with_transparent` returns a Unified Address
//! whose transparent receiver is populated.

use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::TempWalletPath;
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn derive_address_populates_transparent() -> Result<(), TestError> {
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

    // The upstream BIP-44 transparent gap limit defaults to 10 and pre-generates the gap
    // ahead of the reserved address, so a single
    // `derive_next_address_with_transparent` on a fresh wallet exhausts the
    // safe-reservation budget. Further calls require an intervening on-chain output to a
    // reserved address. The regression we care about is whether the first call returns a
    // UA whose transparent receiver is populated.
    let ua = wallet
        .derive_next_address_with_transparent(account_id)
        .await?;
    assert!(
        ua.transparent().is_some(),
        "derive_next_address_with_transparent returned a UA without a transparent receiver"
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
