//! README quick-start example.

#![allow(
    clippy::print_stdout,
    reason = "the quick start prints its sync outcome"
)]

use zally_chain::{ZinderChainSource, ZinderRemoteOptions};
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions};
use zally_wallet::Wallet;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let network = Network::Testnet;
    let chain = ZinderChainSource::connect_remote(ZinderRemoteOptions {
        endpoint: "http://127.0.0.1:19101".into(),
        network,
    })?;
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path("wallet.age".into()));
    let storage = Sqlite::new(SqliteOptions::for_network(network, "wallet.db".into()));

    // Birthday: the height the wallet starts scanning from. Use a recent
    // chain-tip height for a fresh wallet; scanning starts there.
    let birthday_height = 4_150_000;
    let (wallet, _account_id, mnemonic) = Wallet::builder(network, sealing, storage)
        .create(&chain, BlockHeight::from(birthday_height))
        .await?;
    // The mnemonic is the only backup of the seed; store it securely.
    let _ = mnemonic;

    let outcome = wallet.sync(&chain).await?;
    println!("scanned to {}", outcome.scanned_to_height);
    Ok(())
}
