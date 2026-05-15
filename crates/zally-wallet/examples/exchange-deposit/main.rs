//! Exchange deposit cookbook example.
//!
//! Bootstraps a wallet, derives one fresh Unified Address per customer, and subscribes to
//! the wallet event stream so deposits and confirmations land as `WalletEvent` notifications.
//!
//! ```sh
//! cargo run --example exchange-deposit
//! ```
//!
//! The example uses the testkit `MockChainSource` and `MockSubmitter`. Operators swap in
//! their preferred `ChainSource` and `Submitter` implementations in production.

use std::io;

use tempfile::TempDir;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zally_core::{BlockHeight, Network};
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

    let network = Network::regtest();
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

    let params = network.to_parameters();
    let customer_addresses = vec!["customer-001", "customer-002", "customer-003"];
    for customer_id in customer_addresses {
        let ua = wallet.derive_next_address(account_id).await?;
        info!(
            target: "zally::example",
            event = "deposit_address_issued",
            customer_id,
            unified_address = %ua.encode(&params),
            "issued fresh deposit address"
        );
    }

    let _events = wallet.observe();
    let chain = MockChainSource::new(network);
    chain.handle().advance_tip(BlockHeight::from(120));
    let outcome = wallet.sync(&chain).await?;
    info!(
        target: "zally::example",
        event = "sync_complete",
        scanned_to_height = outcome.scanned_to_height.as_u32(),
        block_count = outcome.block_count,
        "sync complete"
    );

    let metrics = wallet.metrics_snapshot().await?;
    info!(
        target: "zally::example",
        event = "metrics_snapshot",
        ?metrics,
        "wallet metrics snapshot"
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
