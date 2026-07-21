//! End-to-end Ironwood sync validation against a live zinder endpoint.
//!
//! Creates a fresh wallet with a birthday `ZALLY_BIRTHDAY_DEPTH` blocks below the tip
//! (default 2000, spanning post-NU6.3 blocks on testnet), then runs the sync driver for
//! `ZALLY_PROBE_SECONDS` (default 300) and reports every snapshot transition: sync
//! outcomes, subtree-root backfill behavior, tree-root check results, reorg rewinds, and
//! repair-ladder activity. Fails unless the wallet reaches the chain tip.
//!
//! ```sh
//! ZINDER_ENDPOINT=http://127.0.0.1:9301 \
//!   ZALLY_NETWORK=testnet \
//!   ZALLY_BIRTHDAY_DEPTH=2000 \
//!   ZALLY_PROBE_SECONDS=300 \
//!   cargo run --release --example e2e-ironwood-probe --features zinder
//! ```

use std::env;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::info;
use tracing_subscriber::EnvFilter;
use zally_chain::{ChainSource, ZinderChainSource, ZinderRemoteOptions};
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions, WalletStorage};
use zally_wallet::{SyncDriver, SyncDriverOptions, SyncHandle, Wallet, WalletError};

#[tokio::main]
async fn main() -> Result<(), ProbeError> {
    init_tracing();

    let settings = ProbeSettings::from_env()?;
    let chain = connect_chain(&settings)?;
    let tip = chain
        .current_epoch()
        .await
        .map_err(ProbeError::Chain)?
        .settled_tip()
        .height;
    let birthday_height = settings.birthday_height(tip);
    log_probe_start(&settings, tip, birthday_height);

    let probe_wallet = open_probe_wallet(&settings, &chain, birthday_height).await?;
    let handle = start_sync(probe_wallet.wallet.clone(), chain)?;
    let has_reached_tip = observe_until_deadline(&handle, settings.probe_seconds).await;
    log_final_roots(&probe_wallet.storage).await?;

    handle.close().await?;
    require_reached_tip(has_reached_tip)?;
    info!(target: "zally::e2e", event = "probe_complete", "end-to-end probe complete");
    Ok(())
}

struct ProbeSettings {
    endpoint: String,
    network: Network,
    birthday_depth_blocks: u32,
    birthday_height: Option<BlockHeight>,
    probe_seconds: u64,
    sealed_dir: PathBuf,
}

impl ProbeSettings {
    fn from_env() -> Result<Self, ProbeError> {
        Ok(Self {
            endpoint: get_required_env("ZINDER_ENDPOINT")?,
            network: parse_network()?,
            birthday_depth_blocks: get_env_u32("ZALLY_BIRTHDAY_DEPTH", 2_000),
            birthday_height: get_env_u32_optional("ZALLY_BIRTHDAY_HEIGHT").map(BlockHeight::from),
            probe_seconds: get_env_u64("ZALLY_PROBE_SECONDS", 300),
            sealed_dir: PathBuf::from(get_required_env("ZALLY_SEALED_DIR")?),
        })
    }

    fn birthday_height(&self, tip: BlockHeight) -> BlockHeight {
        self.birthday_height.unwrap_or_else(|| {
            BlockHeight::from(
                tip.as_u32()
                    .saturating_sub(self.birthday_depth_blocks)
                    .max(1),
            )
        })
    }
}

struct ProbeWallet {
    wallet: Wallet,
    storage: Sqlite,
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();
}

fn get_required_env(name: &'static str) -> Result<String, ProbeError> {
    env::var(name).map_err(|_| ProbeError::MissingEnv(name))
}

fn get_env_u32(name: &'static str, fallback: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(fallback)
}

fn get_env_u32_optional(name: &'static str) -> Option<u32> {
    env::var(name).ok().and_then(|raw| raw.parse().ok())
}

fn get_env_u64(name: &'static str, fallback: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(fallback)
}

fn parse_network() -> Result<Network, ProbeError> {
    match env::var("ZALLY_NETWORK").as_deref() {
        Ok("testnet") => Ok(Network::Testnet),
        Ok("regtest") | Err(_) => Ok(Network::regtest()),
        Ok(other) => Err(ProbeError::UnknownNetwork(other.to_owned())),
    }
}

fn connect_chain(settings: &ProbeSettings) -> Result<ZinderChainSource, ProbeError> {
    let chain = ZinderChainSource::connect_remote(ZinderRemoteOptions {
        endpoint: settings.endpoint.clone(),
        network: settings.network,
    })
    .map_err(ProbeError::Chain)?;
    Ok(chain)
}

fn log_probe_start(settings: &ProbeSettings, tip: BlockHeight, birthday_height: BlockHeight) {
    info!(
        target: "zally::e2e",
        event = "probe_starting",
        endpoint = %settings.endpoint,
        tip_height = tip.as_u32(),
        birthday_height = birthday_height.as_u32(),
        probe_seconds = settings.probe_seconds,
        "starting the Ironwood end-to-end probe"
    );
}

async fn open_probe_wallet(
    settings: &ProbeSettings,
    chain: &ZinderChainSource,
    birthday_height: BlockHeight,
) -> Result<ProbeWallet, ProbeError> {
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(
        settings.sealed_dir.join("wallet.age"),
    ));
    let storage = Sqlite::new(SqliteOptions::for_network(
        settings.network,
        settings.sealed_dir.join("wallet.db"),
    ));
    let (wallet, _account_id) = Wallet::builder(settings.network, sealing, storage.clone())
        .open_or_create_account(chain, birthday_height)
        .await?;
    Ok(ProbeWallet { wallet, storage })
}

fn start_sync(wallet: Wallet, chain: ZinderChainSource) -> Result<SyncHandle, ProbeError> {
    let chain_source: Arc<dyn ChainSource> = Arc::new(chain);
    let driver = SyncDriver::new(
        wallet,
        chain_source,
        SyncDriverOptions::default()
            .with_poll_interval_ms(1_000)
            .with_max_sync_iterations_per_wake_count(16),
    )?;
    Ok(driver.sync_continuously())
}

async fn observe_until_deadline(handle: &SyncHandle, probe_seconds: u64) -> bool {
    let mut snapshots = handle.observe_status();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(probe_seconds);
    let mut has_reached_tip = false;
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => break,
            snapshot = snapshots.next() => {
                let Some(snapshot) = snapshot else { break };
                info!(
                    target: "zally::e2e",
                    event = "probe_snapshot",
                    phase = ?snapshot.phase,
                    scanned_height = snapshot.scanned_height.map(BlockHeight::as_u32),
                    visible_tip = snapshot.visible_tip_height.map(BlockHeight::as_u32),
                    settled_tip = snapshot.settled_tip_height.map(BlockHeight::as_u32),
                    lag_blocks = snapshot.lag_blocks,
                    fault = snapshot.last_fault.as_ref().map(|fault| fault.reason.clone()),
                    "sync driver transition"
                );
                if !has_reached_tip
                    && let (Some(scanned), Some(visible_tip)) =
                        (snapshot.scanned_height, snapshot.visible_tip_height)
                    && scanned >= visible_tip
                {
                    has_reached_tip = true;
                    info!(
                        target: "zally::e2e",
                        event = "probe_reached_tip",
                        scanned_height = scanned.as_u32(),
                        "wallet reached the visible tip; observing for reorgs"
                    );
                }
            }
        }
    }
    has_reached_tip
}

async fn log_final_roots(storage: &Sqlite) -> Result<(), ProbeError> {
    let roots = storage.commitment_tree_roots().await?;
    info!(
        target: "zally::e2e",
        event = "probe_final_roots",
        sapling = %roots.sapling.map_or_else(String::new, hex::encode),
        orchard = %roots.orchard.map_or_else(String::new, hex::encode),
        ironwood = %roots.ironwood.map_or_else(String::new, hex::encode),
        "final wallet commitment-tree roots"
    );
    Ok(())
}

fn require_reached_tip(has_reached_tip: bool) -> Result<(), ProbeError> {
    if has_reached_tip {
        Ok(())
    } else {
        Err(ProbeError::Probe(
            "wallet never reached the chain tip within the probe window".to_owned(),
        ))
    }
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
