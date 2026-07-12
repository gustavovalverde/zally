//! Orchard commitment-tree root convergence from a legacy testnet birthday.
//!
//! A fresh wallet with birthday 4,050,200 rebuilds its note-commitment trees from
//! public chain data alone, so an Orchard root that never matches the chain's tree
//! state reproduces deterministically without funded notes. Each sync step captures
//! the wallet's checkpoint roots next to the chain's tree state at the same height.

use zally_chain::{ChainSource, ChainState, ZinderChainSource, ZinderRemoteOptions};
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{CommitmentTreeRoots, Sqlite, SqliteOptions, WalletStorage};
use zally_testkit::{
    LiveTestError, TempWalletPath, init, require_live, require_network, require_zinder_endpoint,
};
use zally_wallet::{SyncOutcome, Wallet, WalletError};

const LEGACY_BIRTHDAY_HEIGHT: u32 = 4_050_200;
const DEFAULT_TARGET_FULLY_SCANNED_HEIGHT: u32 = 4_058_000;
const SCAN_TARGET_ENV: &str = "ZALLY_TEST_SCAN_TARGET";

fn target_fully_scanned_height() -> u32 {
    std::env::var(SCAN_TARGET_ENV)
        .ok()
        .and_then(|raw| raw.replace('_', "").parse().ok())
        .unwrap_or(DEFAULT_TARGET_FULLY_SCANNED_HEIGHT)
}

fn max_sync_iterations(target: u32) -> u32 {
    target.saturating_sub(LEGACY_BIRTHDAY_HEIGHT) / 900 + 30
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn orchard_root_converges_from_legacy_birthday() -> Result<(), TestError> {
    let _guard = init();
    require_live()?;
    let network = require_testnet()?;
    let endpoint = require_zinder_endpoint()?;
    let chain = ZinderChainSource::connect_remote(ZinderRemoteOptions { endpoint, network })?;

    log_chain_frontier(&chain, BlockHeight::from(LEGACY_BIRTHDAY_HEIGHT - 1)).await?;
    log_chain_frontier(&chain, BlockHeight::from(LEGACY_BIRTHDAY_HEIGHT)).await?;

    let temp = TempWalletPath::create()?;
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(temp.seed_path()));
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    let (wallet, _account_id, _mnemonic) = Wallet::builder(network, sealing, storage.clone())
        .create(&chain, BlockHeight::from(LEGACY_BIRTHDAY_HEIGHT))
        .await?;

    let target_height = target_fully_scanned_height();
    let max_iterations = max_sync_iterations(target_height);
    let mut previous_roots: Option<CommitmentTreeRoots> = None;
    let mut last_checkpoint_height: Option<BlockHeight> = None;

    for iteration in 1..=max_iterations {
        let outcome = match wallet.sync(&chain).await {
            Ok(outcome) => outcome,
            Err(source) => {
                let wallet_roots = storage.commitment_tree_roots().await?;
                tracing::error!(
                    target: "zally::live",
                    event = "legacy_birthday_sync_faulted",
                    iteration,
                    error = %source,
                    wallet_sapling = %hex_or_empty(wallet_roots.sapling),
                    wallet_orchard = %hex_or_empty(wallet_roots.orchard),
                    wallet_ironwood = %hex_or_empty(wallet_roots.ironwood),
                    "sync faulted before the wallet converged"
                );
                return Err(TestError::SyncFaulted { iteration, source });
            }
        };
        let wallet_roots = storage.commitment_tree_roots().await?;
        let fully_scanned = storage.fully_scanned_height().await?;
        let chain_state = chain_frontier_at(&chain, outcome.scanned_to_height).await?;
        log_root_capture(RootCapture {
            iteration,
            outcome: &outcome,
            fully_scanned,
            wallet_roots,
            chain_state: &chain_state,
            previous_roots,
        });
        previous_roots = Some(wallet_roots);
        last_checkpoint_height = Some(outcome.scanned_to_height);
        if fully_scanned.is_some_and(|height| height.as_u32() >= target_height) {
            break;
        }
    }

    let fully_scanned = storage.fully_scanned_height().await?;
    if fully_scanned.is_none_or(|height| height.as_u32() < target_height) {
        return Err(TestError::TargetNotReached {
            target: target_height,
            iterations: max_iterations,
            reached: fully_scanned.map(BlockHeight::as_u32),
        });
    }
    let checkpoint_height = last_checkpoint_height.ok_or(TestError::TargetNotReached {
        target: target_height,
        iterations: max_iterations,
        reached: None,
    })?;
    require_converged_roots(&chain, &storage, checkpoint_height).await
}

async fn require_converged_roots(
    chain: &ZinderChainSource,
    storage: &Sqlite,
    checkpoint_height: BlockHeight,
) -> Result<(), TestError> {
    let wallet_roots = storage.commitment_tree_roots().await?;
    let chain_state = chain_frontier_at(chain, checkpoint_height).await?;
    let chain_sapling = chain_state.final_sapling_tree().root().to_bytes();
    let chain_orchard = chain_state.final_orchard_tree().root().to_bytes();
    if wallet_roots.orchard.is_none() {
        return Err(TestError::OrchardRootMissing {
            height: checkpoint_height.as_u32(),
            chain_tree_size: chain_state.final_orchard_tree().tree_size(),
        });
    }
    let sapling_matches = wallet_roots.sapling == Some(chain_sapling);
    let orchard_matches = wallet_roots.orchard == Some(chain_orchard);
    if sapling_matches && orchard_matches {
        tracing::info!(
            target: "zally::live",
            event = "legacy_birthday_roots_converged",
            height = checkpoint_height.as_u32(),
            sapling = %hex::encode(chain_sapling),
            orchard = %hex::encode(chain_orchard),
            "wallet commitment-tree roots converged with the chain"
        );
        return Ok(());
    }
    Err(TestError::RootsDiverged {
        height: checkpoint_height.as_u32(),
        wallet_sapling: hex_or_empty(wallet_roots.sapling),
        chain_sapling: hex::encode(chain_sapling),
        wallet_orchard: hex_or_empty(wallet_roots.orchard),
        chain_orchard: hex::encode(chain_orchard),
    })
}

#[derive(Clone, Copy)]
struct RootCapture<'a> {
    iteration: u32,
    outcome: &'a SyncOutcome,
    fully_scanned: Option<BlockHeight>,
    wallet_roots: CommitmentTreeRoots,
    chain_state: &'a ChainState,
    previous_roots: Option<CommitmentTreeRoots>,
}

fn log_root_capture(capture: RootCapture<'_>) {
    let RootCapture {
        iteration,
        outcome,
        fully_scanned,
        wallet_roots,
        chain_state,
        previous_roots,
    } = capture;
    let chain_sapling = chain_state.final_sapling_tree().root().to_bytes();
    let chain_orchard = chain_state.final_orchard_tree().root().to_bytes();
    let sapling_match = wallet_roots.sapling.map(|root| root == chain_sapling);
    let orchard_match = wallet_roots.orchard.map(|root| root == chain_orchard);
    let orchard_changed = previous_roots.map(|previous| previous.orchard != wallet_roots.orchard);
    tracing::info!(
        target: "zally::live",
        event = "legacy_birthday_root_capture",
        iteration,
        scanned_from = outcome.scanned_from_height.as_u32(),
        scanned_to = outcome.scanned_to_height.as_u32(),
        block_count = outcome.block_count,
        fully_scanned = ?fully_scanned.map(BlockHeight::as_u32),
        sapling_match = ?sapling_match,
        orchard_match = ?orchard_match,
        orchard_changed = ?orchard_changed,
        wallet_sapling = %hex_or_empty(wallet_roots.sapling),
        chain_sapling = %hex::encode(chain_sapling),
        wallet_orchard = %hex_or_empty(wallet_roots.orchard),
        chain_orchard = %hex::encode(chain_orchard),
        wallet_ironwood = %hex_or_empty(wallet_roots.ironwood),
        chain_sapling_tree_size = chain_state.final_sapling_tree().tree_size(),
        chain_orchard_tree_size = chain_state.final_orchard_tree().tree_size(),
        "wallet vs chain commitment-tree roots after one sync step"
    );
}

async fn log_chain_frontier(
    chain: &ZinderChainSource,
    height: BlockHeight,
) -> Result<(), TestError> {
    let chain_state = chain_frontier_at(chain, height).await?;
    tracing::info!(
        target: "zally::live",
        event = "legacy_birthday_chain_frontier",
        height = height.as_u32(),
        sapling_root = %hex::encode(chain_state.final_sapling_tree().root().to_bytes()),
        sapling_tree_size = chain_state.final_sapling_tree().tree_size(),
        orchard_root = %hex::encode(chain_state.final_orchard_tree().root().to_bytes()),
        orchard_tree_size = chain_state.final_orchard_tree().tree_size(),
        ironwood_tree_size = chain_state.final_ironwood_tree().tree_size(),
        "chain tree state at the birthday boundary"
    );
    Ok(())
}

async fn chain_frontier_at(
    chain: &ZinderChainSource,
    height: BlockHeight,
) -> Result<ChainState, TestError> {
    let tree_state = chain.tree_state_at(height).await?;
    Ok(tree_state.to_chain_state()?)
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non_exhaustive Network maps every non-testnet variant to the same gate error"
)]
fn require_testnet() -> Result<Network, TestError> {
    match require_network()? {
        Network::Testnet => Ok(Network::Testnet),
        _ => Err(TestError::TestnetRequired),
    }
}

fn hex_or_empty(root: Option<[u8; 32]>) -> String {
    root.map_or_else(String::new, hex::encode)
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("live gate error: {0}")]
    Live(#[from] LiveTestError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("chain source error: {0}")]
    Chain(#[from] zally_chain::ChainSourceError),
    #[error("storage error: {0}")]
    Storage(#[from] zally_storage::StorageError),
    #[error("this regression probe requires ZALLY_NETWORK=testnet")]
    TestnetRequired,
    #[error("sync faulted at iteration {iteration}: {source}")]
    SyncFaulted {
        iteration: u32,
        #[source]
        source: WalletError,
    },
    #[error(
        "wallet never fully scanned to {target} within {iterations} sync iterations \
         (reached {reached:?})"
    )]
    TargetNotReached {
        target: u32,
        iterations: u32,
        reached: Option<u32>,
    },
    #[error(
        "wallet orchard root is absent at height {height} even though the chain orchard tree \
         holds {chain_tree_size} leaves; no orchard leaf was ever appended"
    )]
    OrchardRootMissing { height: u32, chain_tree_size: u64 },
    #[error(
        "commitment-tree roots diverged at height {height}: wallet sapling {wallet_sapling}, \
         chain sapling {chain_sapling}, wallet orchard {wallet_orchard}, \
         chain orchard {chain_orchard}"
    )]
    RootsDiverged {
        height: u32,
        wallet_sapling: String,
        chain_sapling: String,
        wallet_orchard: String,
        chain_orchard: String,
    },
}
