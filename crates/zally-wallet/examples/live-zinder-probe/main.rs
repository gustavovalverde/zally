//! Live zinder connectivity probe.
//!
//! Connects to a running `zinder-query` endpoint via [`ZinderChainSource`] and proves the
//! integration by:
//!
//! 1. Reading the chain tip.
//! 2. Driving `Wallet::sync` against the live endpoint and printing the sync outcome.
//! 3. Reading one tree-state artifact at the tip.
//!
//! ```sh
//! ZINDER_ENDPOINT=http://127.0.0.1:9101 \
//!   ZALLY_NETWORK=regtest \
//!   cargo run --example live-zinder-probe --features zinder
//! ```

use std::env;
use std::io;

use tempfile::TempDir;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zally_chain::{ChainSource, ZinderChainSource, ZinderRemoteOptions};
use zally_core::{BlockHeight, Network, PaymentRecipient, Zatoshis};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_wallet::{ProposalPlan, Wallet, WalletError};

#[tokio::main]
async fn main() -> Result<(), ExampleError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let endpoint =
        env::var("ZINDER_ENDPOINT").map_err(|_| ExampleError::MissingEnv("ZINDER_ENDPOINT"))?;
    let network = network_from_env()?;

    info!(
        target: "zally::example",
        event = "live_zinder_probe_starting",
        %endpoint,
        ?network,
        "connecting to live zinder endpoint"
    );

    let chain = ZinderChainSource::connect_remote(ZinderRemoteOptions {
        endpoint: endpoint.clone(),
        network,
    })
    .map_err(ExampleError::Chain)?;

    let tip = chain.chain_tip().await.map_err(ExampleError::Chain)?;
    info!(
        target: "zally::example",
        event = "live_zinder_tip_observed",
        tip_height = tip.as_u32(),
        "live zinder reports chain tip"
    );

    let temp = TempDir::new()?;
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(
        temp.path().join("wallet.age"),
    ));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.path().join("wallet.db"),
    ));
    let (wallet, account_id, _mnemonic) = Wallet::create(
        &chain,
        network,
        sealing,
        storage,
        BlockHeight::from(tip.as_u32().saturating_sub(1).max(1)),
    )
    .await?;

    let outcome = wallet.sync(&chain).await?;
    info!(
        target: "zally::example",
        event = "live_zinder_sync_outcome",
        scanned_to_height = outcome.scanned_to_height.as_u32(),
        block_count = outcome.block_count,
        "Wallet::sync completed against live zinder"
    );

    let tree_state = chain
        .tree_state_at(tip)
        .await
        .map_err(ExampleError::Chain)?;
    info!(
        target: "zally::example",
        event = "live_zinder_tree_state",
        height = tree_state.height,
        sapling_tree_bytes = tree_state.sapling_tree.len(),
        orchard_tree_bytes = tree_state.orchard_tree.len(),
        "live zinder served tree state at tip (JSON->proto translated)"
    );

    probe_live_propose(&wallet, account_id, network).await?;
    probe_live_pczt_cycle(&wallet, account_id, network).await?;

    Ok(())
}

async fn probe_live_propose(
    wallet: &Wallet,
    account_id: zally_core::AccountId,
    network: Network,
) -> Result<(), ExampleError> {
    let recipient_ua = wallet.derive_next_address(account_id).await?;
    let encoded = recipient_ua.encode(&network.to_parameters());
    let plan = ProposalPlan::conventional(
        account_id,
        PaymentRecipient::UnifiedAddress {
            encoded: encoded.clone(),
            network,
        },
        Zatoshis::try_from(10_000_u64)
            .map_err(|err| ExampleError::Probe(format!("zatoshi build failed: {err}")))?,
        None,
    );
    info!(
        target: "zally::example",
        event = "live_zinder_propose_attempt",
        recipient_ua = %encoded,
        "Wallet::propose driving live propose_transfer against the freshly-scanned WalletDb"
    );
    match wallet.propose(plan).await {
        Ok(proposal) => info!(
            target: "zally::example",
            event = "live_zinder_propose_success",
            total_zat = proposal.total_zat().as_u64(),
            fee_zat = proposal.fee_zat().as_u64(),
            "Wallet::propose returned a real proposal (wallet has funds)"
        ),
        Err(WalletError::InsufficientBalance {
            requested_zat,
            spendable_zat,
        }) => info!(
            target: "zally::example",
            event = "live_zinder_propose_insufficient_balance",
            requested_zat,
            spendable_zat,
            "Wallet::propose hit InsufficientBalance against the live empty wallet (expected)"
        ),
        Err(other) => info!(
            target: "zally::example",
            event = "live_zinder_propose_other_error",
            reason = %other,
            "Wallet::propose surfaced a non-balance error"
        ),
    }
    Ok(())
}

async fn probe_live_pczt_cycle(
    wallet: &Wallet,
    account_id: zally_core::AccountId,
    network: Network,
) -> Result<(), ExampleError> {
    let recipient_ua = wallet.derive_next_address(account_id).await?;
    let encoded = recipient_ua.encode(&network.to_parameters());
    let plan = ProposalPlan::conventional(
        account_id,
        PaymentRecipient::UnifiedAddress {
            encoded: encoded.clone(),
            network,
        },
        Zatoshis::try_from(10_000_u64)
            .map_err(|err| ExampleError::Probe(format!("zatoshi build failed: {err}")))?,
        None,
    );
    info!(
        target: "zally::example",
        event = "live_zinder_propose_pczt_attempt",
        recipient_ua = %encoded,
        "Wallet::propose_pczt driving create_pczt_from_proposal against the freshly-scanned WalletDb"
    );
    match wallet.propose_pczt(plan).await {
        Ok(pczt) => info!(
            target: "zally::example",
            event = "live_zinder_propose_pczt_success",
            pczt_bytes = pczt.as_bytes().len(),
            "Wallet::propose_pczt returned a real PCZT (wallet has funds)"
        ),
        Err(WalletError::InsufficientBalance {
            requested_zat,
            spendable_zat,
        }) => info!(
            target: "zally::example",
            event = "live_zinder_propose_pczt_insufficient_balance",
            requested_zat,
            spendable_zat,
            "Wallet::propose_pczt hit InsufficientBalance against the live empty wallet (expected)"
        ),
        Err(other) => info!(
            target: "zally::example",
            event = "live_zinder_propose_pczt_other_error",
            reason = %other,
            "Wallet::propose_pczt surfaced a non-balance error"
        ),
    }
    Ok(())
}

fn network_from_env() -> Result<Network, ExampleError> {
    match env::var("ZALLY_NETWORK").as_deref() {
        Ok("mainnet") => Ok(Network::Mainnet),
        Ok("testnet") => Ok(Network::Testnet),
        Ok("regtest") | Err(_) => Ok(Network::regtest()),
        Ok(other) => Err(ExampleError::UnknownNetwork(other.to_owned())),
    }
}

#[derive(Debug, thiserror::Error)]
enum ExampleError {
    #[error("required environment variable {0} is not set")]
    MissingEnv(&'static str),
    #[error("unknown ZALLY_NETWORK value: {0}")]
    UnknownNetwork(String),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("chain source error: {0}")]
    Chain(zally_chain::ChainSourceError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("probe error: {0}")]
    Probe(String),
}
