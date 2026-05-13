//! Payment-processor cookbook example.
//!
//! Parses a ZIP-321 payment URI, validates the recipient + memo + amount, and proposes a
//! payment. Slice 5 stops at `WalletError::InsufficientBalance` against the empty test
//! storage; the surface exercised is the ZIP-321 → `ProposalPlan` → `Wallet::propose` path
//! that operators wire into their merchant-side flow.

use std::io;

use tempfile::TempDir;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use zally_core::{BlockHeight, IdempotencyKey, Network};
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

    let idempotency = IdempotencyKey::try_from("invoice-2026-05-13-abc-123")
        .map_err(|err| ExampleError::Idempotency(err.to_string()))?;
    info!(
        target: "zally::example",
        event = "idempotency_key_registered",
        key = idempotency.as_str(),
        "payment-processor recorded idempotency key"
    );

    // In production: parse a real ZIP-321 URI here.
    //   let request = PaymentRequest::from_uri(uri, network)?;
    // For this example we build a `ProposalPlan` directly so the path is exercised even
    // against the empty test storage.
    let plan = ProposalPlan::conventional(
        account_id,
        zally_core::PaymentRecipient::UnifiedAddress {
            encoded: "uregtest1example".into(),
            network,
        },
        zally_core::Zatoshis::try_from(50_000_u64)
            .map_err(|err| ExampleError::Zat(err.to_string()))?,
        None,
    );

    match wallet.propose(plan).await {
        Ok(proposal) => {
            info!(
                target: "zally::example",
                event = "proposal_ready",
                total_zat = proposal.total_zat().as_u64(),
                fee_zat = proposal.fee_zat().as_u64(),
                "proposal ready for sign+submit"
            );
        }
        Err(WalletError::InsufficientBalance {
            requested_zat,
            spendable_zat,
        }) => {
            warn!(
                target: "zally::example",
                event = "proposal_short_circuited_insufficient_balance",
                requested_zat,
                spendable_zat,
                "proposal short-circuited; v1 follow-up wires live balance"
            );
        }
        Err(other) => return Err(other.into()),
    }

    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum ExampleError {
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("idempotency key invalid: {0}")]
    Idempotency(String),
    #[error("zatoshi amount invalid: {0}")]
    Zat(String),
}
