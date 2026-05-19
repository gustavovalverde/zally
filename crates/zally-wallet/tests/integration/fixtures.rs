//! Shared fixture for the integration tests.
//!
//! Every T1 test sets up the same wallet shape: a temp directory holding an age-sealed seed
//! and a sqlite wallet store, a regtest mock chain source, and a fresh wallet rooted at
//! birthday height 1. `create_test_wallet` lifts this boilerplate out of every test so the
//! tests focus on the behaviour they assert.

use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions};
use zally_testkit::{MockChainSource, TempWalletPath};
use zally_wallet::{Wallet, WalletError, WalletOptions};

/// Result of a [`create_test_wallet`] fixture.
///
/// Holds the temp directory alongside the wallet so the storage file outlives the test (the
/// temp dir is RAII; dropping it deletes the wallet on disk).
pub(crate) struct TestWalletFixture {
    pub temp: TempWalletPath,
    pub wallet: Wallet,
    pub account_id: zally_core::AccountId,
}

/// Constructs a fresh wallet at birthday height 1 on a temp regtest store with default
/// [`WalletOptions`].
pub(crate) async fn create_test_wallet() -> Result<TestWalletFixture, TestWalletError> {
    create_test_wallet_with_options(WalletOptions::default()).await
}

/// Same as [`create_test_wallet`] but with caller-supplied [`WalletOptions`]. Used by tests
/// that need a non-default pending-broadcast window.
pub(crate) async fn create_test_wallet_with_options(
    options: WalletOptions,
) -> Result<TestWalletFixture, TestWalletError> {
    let temp = TempWalletPath::create()?;
    let network = Network::regtest();
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    let chain = MockChainSource::new(network);
    let (wallet, account_id, _mnemonic) = Wallet::builder(network, sealing, storage)
        .with_options(options)
        .create(&chain, BlockHeight::from(1))
        .await?;
    Ok(TestWalletFixture {
        temp,
        wallet,
        account_id,
    })
}

/// Error type returned by the fixture. Captures every error class a test setup can hit.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TestWalletError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
}
