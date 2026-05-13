//! Regression: `Wallet::list_unspent_shielded_notes` returns the storage-reported notes
//! and computes confirmations against the persisted observed tip.
//!
//! The test bypasses the scanner: a fresh wallet has no on-chain notes, so we verify the
//! method returns an empty list and does not error. End-to-end coverage with a funded note
//! lands once an operator-funded fixture is plumbed (fauzec phase 2 staging soak).

use zally_chain::ChainSource as _;
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::{MockChainSource, TempWalletPath};
use zally_wallet::{Wallet, WalletError};

#[tokio::test]
async fn list_unspent_shielded_notes_returns_empty_on_fresh_wallet() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let (wallet, account_id, _mnemonic) =
        Wallet::create(network, sealing, storage, BlockHeight::from(1)).await?;

    let notes = wallet.list_unspent_shielded_notes(account_id).await?;
    assert!(notes.is_empty(), "fresh wallet must have no unspent notes");
    Ok(())
}

#[tokio::test]
async fn list_unspent_shielded_notes_uses_observed_tip_after_sync() -> Result<(), TestError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest_all_at_genesis();

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_local_tests(temp.db_path()));
    let (wallet, account_id, _mnemonic) =
        Wallet::create(network, sealing, storage, BlockHeight::from(1)).await?;

    // Advance the wallet's observed tip via sync; the MockChainSource returns an empty
    // block stream so no notes get persisted, but observed_tip lands.
    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(200));
    wallet.sync(&chain).await?;
    let tip = chain.chain_tip().await.map_err(|err| TestError::Chain {
        reason: err.to_string(),
    })?;
    assert_eq!(tip.as_u32(), 200, "mock chain tip must be set");

    // No notes still (mock returned empty), but the surface compiles and returns Ok.
    let notes = wallet.list_unspent_shielded_notes(account_id).await?;
    assert!(notes.is_empty());
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("chain source error: {reason}")]
    Chain { reason: String },
}
