//! Slice 1 cookbook example.
//!
//! Creates a wallet, derives the first Unified Address, drops the wallet handle, re-opens
//! from the sealed seed, derives another Unified Address, and asserts the second address
//! differs from the first (the wallet walks forward through diversifier indices per
//! ZIP-316).
//!
//! Run against regtest (no live node needed in Slice 1):
//!
//! ```sh
//! cargo run --example open-wallet
//! ```
//!
//! To exercise the plaintext-seed branch (NEVER in production):
//!
//! ```sh
//! cargo run --example open-wallet --features unsafe_plaintext_seed
//! ```

use std::io;
use std::path::PathBuf;

use tempfile::TempDir;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_wallet::{Wallet, WalletError};
use zcash_keys::address::UnifiedAddress;

#[tokio::main]
async fn main() -> Result<(), ExampleError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let network = Network::regtest_all_at_genesis();
    let temp = TempDir::new()?;
    let seed_path = temp.path().join("wallet.age");
    let db_path = temp.path().join("wallet.db");

    let ua_first = bootstrap_wallet(network, &seed_path, &db_path).await?;
    let ua_second = reopen_wallet(network, &seed_path, &db_path).await?;

    let params = network.to_parameters();
    let first_encoded = ua_first.encode(&params);
    let second_encoded = ua_second.encode(&params);
    info!(
        target: "zally::example",
        event = "round_trip_verified",
        first_address = %first_encoded,
        second_address = %second_encoded,
        "address round-trip verified"
    );
    if first_encoded == second_encoded {
        return Err(ExampleError::AddressDidNotAdvance);
    }

    #[cfg(feature = "unsafe_plaintext_seed")]
    plaintext_demo(network, temp.path()).await?;

    Ok(())
}

async fn bootstrap_wallet(
    network: Network,
    seed_path: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<UnifiedAddress, WalletError> {
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(seed_path.to_path_buf()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        db_path.to_path_buf(),
    ));

    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, account_id, mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;

    // The operator must record the mnemonic out-of-band. Zally does not back it up.
    warn!(
        target: "zally::example",
        event = "mnemonic_pending_capture",
        word_count = mnemonic.word_count(),
        "wallet created; capture the 24-word mnemonic to a tightly-permissioned store now"
    );
    let _phrase = mnemonic.as_phrase();

    let ua = wallet.derive_next_address(account_id).await?;
    info!(
        target: "zally::example",
        event = "address_derived",
        capabilities = ?wallet.capabilities(),
        "derived first Unified Address"
    );
    Ok(ua)
}

async fn reopen_wallet(
    network: Network,
    seed_path: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<UnifiedAddress, WalletError> {
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(seed_path.to_path_buf()));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        db_path.to_path_buf(),
    ));
    let (wallet, account_id) = Wallet::open(network, sealing, storage).await?;
    wallet.derive_next_address(account_id).await
}

#[cfg(feature = "unsafe_plaintext_seed")]
async fn plaintext_demo(network: Network, dir: &std::path::Path) -> Result<(), WalletError> {
    use zally_keys::PlaintextSealing;

    let seed_path = dir.join("UNSAFE.seed");
    let db_path = dir.join("UNSAFE.db");
    let sealing = PlaintextSealing::new(seed_path);
    let storage =
        SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(network, db_path));
    let chain = zally_testkit::MockChainSource::new(network);
    let (wallet, account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;
    let _ua = wallet.derive_next_address(account_id).await?;
    warn!(
        target: "zally::example",
        event = "plaintext_seed_demo_complete",
        "plaintext_seed branch demonstrated; never use in production"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum ExampleError {
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("address did not advance across re-open; ZIP-316 next-available semantics broken")]
    AddressDidNotAdvance,
}

#[allow(
    dead_code,
    reason = "compiler does not see the path-based example helper"
)]
#[doc(hidden)]
#[must_use]
pub fn _resolve_seed_path(dir: &std::path::Path) -> PathBuf {
    dir.join("wallet.age")
}
