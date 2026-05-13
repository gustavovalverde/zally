//! Mining-payout cookbook example.
//!
//! Demonstrates the ZIP-213 confirmation-depth rule for coinbase receives. The example:
//!
//! 1. Bootstraps a wallet on regtest.
//! 2. Derives a Unified Address for the operator's mining receiver.
//! 3. Reads the ZIP-213 default depth from [`zally_core::ReceiverPurpose::Mining`] and prints it
//!    alongside the non-coinbase default for contrast.
//! 4. Drives a sync via `MockChainSource` so the example exercises the same `WalletEvent`
//!    plane that a live `ChainSource` would.
//!
//! ```sh
//! cargo run --example mining-payout
//! ```
//!
//! The mining-payout flow is operationally distinct from a hot-dispense (exchange withdrawals)
//! flow because newly mined coinbase outputs cannot be spent for 100 blocks. Operators wire
//! the `confirmation_depth_blocks` into their own bookkeeping; Zally exposes the constant from
//! a single source of truth so it cannot drift.

use std::io;

use tempfile::TempDir;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zally_core::{BlockHeight, Network, ReceiverPurpose};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_testkit::MockChainSource;
use zally_wallet::{Wallet, WalletError};

#[tokio::main]
async fn main() -> Result<(), ExampleError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let network = Network::regtest_all_at_genesis();
    let temp = TempDir::new()?;
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(
        temp.path().join("wallet.age"),
    ));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.path().join("wallet.db"),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    let mining = ReceiverPurpose::Mining;
    let hot_dispense = ReceiverPurpose::HotDispense;
    info!(
        target: "zally::example",
        event = "confirmation_depth_policy",
        mining_depth_blocks = mining.default_confirmation_depth_blocks(),
        hot_dispense_depth_blocks = hot_dispense.default_confirmation_depth_blocks(),
        "ZIP-213 coinbase depth applied to mining receiver; hot-dispense uses single-block depth"
    );

    let params = network.to_parameters();
    let ua = wallet.derive_next_address(account_id).await?;
    info!(
        target: "zally::example",
        event = "mining_receiver_issued",
        unified_address = %ua.encode(&params),
        confirmation_depth_blocks = mining.default_confirmation_depth_blocks(),
        "issued mining receiver"
    );

    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(250));
    let outcome = wallet.sync(&chain).await?;
    info!(
        target: "zally::example",
        event = "sync_complete",
        scanned_to_height = outcome.scanned_to_height.as_u32(),
        block_count = outcome.block_count,
        mature_height = outcome.scanned_to_height.as_u32().saturating_sub(
            mining.default_confirmation_depth_blocks(),
        ),
        "sync complete; mature_height shows the lowest block at which a coinbase from genesis \
         would be spendable"
    );

    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum ExampleError {
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}
