//! Custody-with-PCZT cookbook example.
//!
//! Demonstrates the [`Creator`], [`Signer`], [`Combiner`], and [`Extractor`] role surface for
//! cold custody. The example covers:
//!
//! 1. The network-mismatch guard that every role enforces *before* touching key material.
//! 2. The signer's `NoMatchingKeys` and the extractor's `NotFinalized` short-circuits when a
//!    role is handed a PCZT that its seed cannot authorise or that has not been signed.
//! 3. The Combiner's empty-input rejection (`CombineConflict`) for the FROST/multi-sig path.
//!
//! Operators in production substitute their own `PcztBytes::from_serialized` source: a watch
//! online wallet builds the unsigned PCZT, the offline cold signer adds spend authorisations,
//! and the watch-only side combines + extracts the final transaction.
//!
//! ```sh
//! cargo run --example custody-with-pczt
//! ```

use std::io;

use tempfile::TempDir;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use zally_core::{BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions, Mnemonic, SeedMaterial};
use zally_pczt::{Combiner, Creator, Extractor, PcztBytes, PcztError, Signer};
use zally_storage::{SqliteWalletStorage, SqliteWalletStorageOptions};
use zally_wallet::{Wallet, WalletError};

#[tokio::main]
async fn main() -> Result<(), ExampleError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let network = Network::regtest();
    bootstrap_wallet(network).await?;

    let creator = Creator::new(network);
    let signer = Signer::new(network);
    let combiner = Combiner::new();
    let extractor = Extractor::new();
    info!(
        target: "zally::example",
        event = "pczt_roles_constructed",
        creator_network = ?creator.network(),
        signer_network = ?signer.network(),
        "PCZT role chain ready"
    );

    let online_pczt = PcztBytes::from_serialized(vec![0_u8; 32], network);
    demonstrate_network_guard(&signer).await?;
    demonstrate_sign_short_circuit(&signer, online_pczt.clone()).await?;
    demonstrate_combiner_rejects_empty(&combiner);
    demonstrate_extractor_short_circuit(&extractor, online_pczt);
    Ok(())
}

async fn bootstrap_wallet(network: Network) -> Result<TempDir, ExampleError> {
    let temp = TempDir::new()?;
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(
        temp.path().join("wallet.age"),
    ));
    let storage = SqliteWalletStorage::new(SqliteWalletStorageOptions::for_network(
        network,
        temp.path().join("wallet.db"),
    ));
    let chain = zally_testkit::MockChainSource::new(network);
    let (_wallet, _account_id, _mnemonic) =
        Wallet::create(&chain, network, sealing, storage, BlockHeight::from(1)).await?;
    Ok(temp)
}

async fn demonstrate_network_guard(signer: &Signer) -> Result<(), ExampleError> {
    let mismatched = PcztBytes::from_serialized(vec![0_u8; 32], Network::Mainnet);
    let seed = SeedMaterial::from_mnemonic(&Mnemonic::generate(), "");
    match signer.sign_with_seed(mismatched, &seed).await {
        Err(PcztError::NetworkMismatch {
            pczt_network,
            configured_network,
        }) => {
            info!(
                target: "zally::example",
                event = "network_guard_rejected_mainnet_pczt_for_regtest_signer",
                pczt_network = ?pczt_network,
                configured_network = ?configured_network,
                "signer refused mainnet PCZT routed to regtest signer"
            );
            Ok(())
        }
        Err(other) => Err(other.into()),
        Ok(_) => Err(ExampleError::GuardFailedToReject),
    }
}

async fn demonstrate_sign_short_circuit(
    signer: &Signer,
    pczt: PcztBytes,
) -> Result<(), ExampleError> {
    let seed = SeedMaterial::from_mnemonic(&Mnemonic::generate(), "");
    match signer.sign_with_seed(pczt, &seed).await {
        Err(PcztError::NoMatchingKeys | PcztError::ParseFailed { .. }) => {
            warn!(
                target: "zally::example",
                event = "sign_short_circuited",
                "signer refused: the seed cannot authorise any spend in this PCZT"
            );
            Ok(())
        }
        Err(other) => Err(other.into()),
        Ok(_) => {
            warn!(
                target: "zally::example",
                event = "sign_returned_signed_pczt",
                "signer authorised every matching spend in the PCZT"
            );
            Ok(())
        }
    }
}

fn demonstrate_combiner_rejects_empty(combiner: &Combiner) {
    if let Err(PcztError::CombineConflict { reason }) = combiner.combine(Vec::new()) {
        info!(
            target: "zally::example",
            event = "combiner_rejected_empty_input",
            reason = %reason,
            "combiner refused empty input (FROST quorum requires at least one signed PCZT)"
        );
    }
}

fn demonstrate_extractor_short_circuit(extractor: &Extractor, pczt: PcztBytes) {
    if let Err(PcztError::NotFinalized { reason }) = extractor.extract(pczt) {
        warn!(
            target: "zally::example",
            event = "extract_short_circuited",
            reason = %reason,
            "extractor refused: the PCZT is not finalised"
        );
    }
}

#[derive(Debug, thiserror::Error)]
enum ExampleError {
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("pczt error: {0}")]
    Pczt(#[from] PcztError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("network guard failed to reject mismatched PCZT")]
    GuardFailedToReject,
}
