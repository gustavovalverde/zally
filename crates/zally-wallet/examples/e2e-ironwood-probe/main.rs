//! End-to-end Ironwood sync validation against a live zinder endpoint.
//!
//! Creates a fresh wallet with a birthday `ZALLY_BIRTHDAY_DEPTH` blocks below the tip
//! (default 2000, spanning post-NU6.3 blocks on testnet), then runs the sync driver for
//! `ZALLY_PROBE_SECONDS` (default 300) and reports every snapshot transition: sync
//! outcomes, subtree-root backfill behavior, tree-root check results, reorg rewinds, and
//! repair-ladder activity.
//!
//! ```sh
//! ZINDER_ENDPOINT=http://127.0.0.1:9101 \
//!   ZALLY_NETWORK=testnet \
//!   ZALLY_BIRTHDAY_DEPTH=2000 \
//!   ZALLY_PROBE_SECONDS=300 \
//!   cargo run --release --example e2e-ironwood-probe --features zinder
//! ```

use std::env;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zally_chain::{ChainSource, ZinderChainSource, ZinderRemoteOptions};
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions, WalletStorage};
use zally_wallet::{SyncDriverOptions, Wallet, WalletError};

#[tokio::main]
async fn main() -> Result<(), ProbeError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let endpoint =
        env::var("ZINDER_ENDPOINT").map_err(|_| ProbeError::MissingEnv("ZINDER_ENDPOINT"))?;
    let network = match env::var("ZALLY_NETWORK").as_deref() {
        Ok("testnet") => Network::testnet(),
        Ok("regtest") | Err(_) => Network::regtest(),
        Ok(other) => return Err(ProbeError::UnknownNetwork(other.to_owned())),
    };
    let birthday_depth: u32 = env::var("ZALLY_BIRTHDAY_DEPTH")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(2_000);
    let probe_seconds: u64 = env::var("ZALLY_PROBE_SECONDS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(300);

    let chain = ZinderChainSource::connect_remote(ZinderRemoteOptions {
        endpoint: endpoint.clone(),
        network,
    })
    .map_err(ProbeError::Chain)?;

    let tip = chain.safe_chain_tip().await.map_err(ProbeError::Chain)?;
    let birthday = BlockHeight::from(tip.as_u32().saturating_sub(birthday_depth).max(1));
    info!(
        target: "zally::e2e",
        event = "probe_starting",
        %endpoint,
        tip_height = tip.as_u32(),
        birthday_height = birthday.as_u32(),
        probe_seconds,
        "starting the Ironwood end-to-end probe"
    );

    let temp = TempDir::new()?;
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(
        temp.path().join("wallet.age"),
    ));
    let storage = Sqlite::new(SqliteOptions::for_network(
        network,
        temp.path().join("wallet.db"),
    ));
    let (wallet, _account_id, _mnemonic) = Wallet::builder(network, sealing, storage.clone())
        .create(&chain, birthday)
        .await?;

    let chain: Arc<dyn ChainSource> = Arc::new(chain);
    let driver = wallet
        .clone()
        .spawn_sync_driver(Arc::clone(&chain), SyncDriverOptions::default());
    let mut status = driver.subscribe();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(probe_seconds);
    let mut reached_tip = false;
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => break,
            changed = status.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = status.borrow().clone();
                info!(
                    target: "zally::e2e",
                    event = "probe_snapshot",
                    phase = ?snapshot.phase,
                    scanned_height = snapshot.scanned_height.map(BlockHeight::as_u32),
                    chain_tip = snapshot.chain_tip_height.map(BlockHeight::as_u32),
                    fault = snapshot.last_fault.as_ref().map(|fault| fault.reason.clone()),
                    "sync driver transition"
                );
                if !reached_tip
                    && let (Some(scanned), Some(chain_tip)) =
                        (snapshot.scanned_height, snapshot.chain_tip_height)
                    && scanned >= chain_tip
                {
                    reached_tip = true;
                    info!(
                        target: "zally::e2e",
                        event = "probe_reached_tip",
                        scanned_height = scanned.as_u32(),
                        "wallet reached the chain tip; continuing to observe for reorgs"
                    );
                }
            }
        }
    }

    let roots = storage.commitment_tree_roots().await?;
    info!(
        target: "zally::e2e",
        event = "probe_final_roots",
        sapling = %roots.sapling.map_or_else(String::new, hex::encode),
        orchard = %roots.orchard.map_or_else(String::new, hex::encode),
        ironwood = %roots.ironwood.map_or_else(String::new, hex::encode),
        "final wallet commitment-tree roots"
    );

    driver.close().await;
    if !reached_tip {
        return Err(ProbeError::Probe(
            "wallet never reached the chain tip within the probe window".to_owned(),
        ));
    }
    info!(target: "zally::e2e", event = "probe_complete", "end-to-end probe complete");
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum ProbeError {
    #[error("required environment variable {0} is not set")]
    MissingEnv(&'static str),
    #[error("unknown ZALLY_NETWORK value: {0}")]
    UnknownNetwork(String),
    #[error("chain source error: {0}")]
    Chain(#[from] zally_chain::ChainSourceError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("storage error: {0}")]
    Storage(#[from] zally_storage::StorageError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("probe failure: {0}")]
    Probe(String),
}
