//! `Wallet::open_or_restore` covers the env-var bootstrap path.
//!
//! The sealed seed travels with the operator (env var or out-of-band copy), the persistent
//! volume's storage is empty on first boot, and the wallet still opens.

use std::fs;

use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::TempWalletPath;
use zally_wallet::{Wallet, WalletError};

const SIDECAR_SUFFIX: &str = ".age-identity";

#[tokio::test]
async fn open_or_restore_recovers_account_on_fresh_storage() -> Result<(), TestError> {
    let origin = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(origin.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(
        origin.db_path(),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (_wallet, original_account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    let restored = TempWalletPath::create()?;
    fs::copy(origin.seed_path(), restored.seed_path())?;
    fs::copy(
        sidecar_for(&origin.seed_path()),
        sidecar_for(&restored.seed_path()),
    )?;

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(restored.seed_path()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(
        restored.db_path(),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (restored_wallet, restored_account_id) =
        Wallet::open_or_restore(&chain, network, sealing, storage, BlockHeight::from(1)).await?;
    assert_ne!(
        original_account_id, restored_account_id,
        "AccountId is a fresh per-row UUID; restoring across separate storages must allocate \
         a new one even though the seed material is the same"
    );

    let ua = restored_wallet
        .derive_next_address(restored_account_id)
        .await?
        .encode(&network.to_parameters());
    assert!(
        ua.starts_with("uregtest1"),
        "restored wallet must be capable of deriving a Unified Address: got {ua}"
    );
    Ok(())
}

#[tokio::test]
async fn open_or_restore_is_idempotent_on_warm_storage() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let chain = zally_testkit::MockChainSource::new(network);
    let (_wallet, original_account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let chain = zally_testkit::MockChainSource::new(network);
    let (_, reopened_account_id) =
        Wallet::open_or_restore(&chain, network, sealing, storage, BlockHeight::from(99_999))
            .await?;
    assert_eq!(
        original_account_id, reopened_account_id,
        "second open_or_restore must surface the existing account, ignoring the birthday arg"
    );
    Ok(())
}

#[tokio::test]
async fn open_or_restore_without_seal_returns_no_sealed_seed() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let chain = zally_testkit::MockChainSource::new(network);
    let outcome =
        Wallet::open_or_restore(&chain, network, sealing, storage, BlockHeight::from(1)).await;
    assert!(matches!(outcome, Err(WalletError::NoSealedSeed)));
    Ok(())
}

fn sidecar_for(seed_path: &std::path::Path) -> std::path::PathBuf {
    let mut sidecar = seed_path.as_os_str().to_owned();
    sidecar.push(SIDECAR_SUFFIX);
    std::path::PathBuf::from(sidecar)
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
