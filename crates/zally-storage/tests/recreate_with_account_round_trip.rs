//! `WalletStorage::recreate_with_account` rebuilds the wallet database in place while
//! the deterministic account identity survives: the same seed yields the same
//! `AccountId` before and after the rebuild, handles holding the pre-rebuild id keep
//! working, and every piece of wallet-local state (derived chain state and the zally
//! ledger alike) starts fresh.

use tempfile::TempDir;
use zally_core::{
    BlockHeight, HoldId, IdempotencyKey, IdempotencyKeyError, TxId, Zatoshis, ZatoshisError,
};
use zally_keys::{Mnemonic, SeedMaterial};
use zally_storage::{HoldRecord, Sqlite, SqliteOptions, StorageError, WalletStorage};

#[path = "fixtures/scan_artifact.rs"]
mod scan_artifact;

fn open_storage(temp: &TempDir) -> Sqlite {
    Sqlite::new(SqliteOptions::for_network(
        zally_core::Network::regtest(),
        temp.path().join("wallet.db"),
    ))
}

fn fresh_seed() -> SeedMaterial {
    SeedMaterial::from_mnemonic(&Mnemonic::generate(), "")
}

fn genesis_chain_state() -> zally_core::TreeStateArtifact {
    scan_artifact::genesis_tree_state(zally_core::Network::regtest(), 0, [0; 32])
}

#[tokio::test]
async fn account_id_survives_database_rebuild() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = open_storage(&temp);
    storage.open_or_create().await?;

    let seed = fresh_seed();
    let original_id = storage
        .create_account_for_seed(&seed, genesis_chain_state())
        .await?;
    assert_eq!(
        storage.find_account_for_seed(&seed).await?,
        Some(original_id),
        "lookup by seed must agree with the id returned at creation"
    );

    let rebuilt_id = storage
        .recreate_with_account(&seed, genesis_chain_state())
        .await?;
    assert_eq!(
        original_id, rebuilt_id,
        "the same seed must yield the same AccountId across a database rebuild"
    );

    let address = storage.derive_next_address(original_id).await?;
    assert!(
        address.orchard().is_some(),
        "an account-scoped call with the pre-rebuild id must still succeed"
    );
    Ok(())
}

#[tokio::test]
async fn recreate_drops_derived_and_ledger_state() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = open_storage(&temp);
    storage.open_or_create().await?;

    let seed = fresh_seed();
    let account_id = storage
        .create_account_for_seed(&seed, genesis_chain_state())
        .await?;

    storage
        .record_chain_tips(BlockHeight::from(50), BlockHeight::from(40))
        .await?;
    let key = IdempotencyKey::try_from("idempotency-rebuild-1".to_owned())?;
    storage
        .record_idempotent_submission(key.clone(), TxId::from_bytes([0xAB; 32]))
        .await?;
    storage
        .create_hold(HoldRecord {
            hold_id: HoldId::new(),
            request_id: IdempotencyKey::try_from("request-rebuild-1".to_owned())?,
            idempotency_key: key.clone(),
            account_id,
            amount_zat: Zatoshis::try_from(10_000_000_u64)?,
            spendable_for_check_zat: Zatoshis::try_from(100_000_000_u64)?,
            locked_notes: Vec::new(),
            reserved_at_ms: 1_700_000_000_000,
        })
        .await?;

    let mut rebuilt_anchor =
        scan_artifact::genesis_tree_state(zally_core::Network::regtest(), 9, [1; 32]);
    rebuilt_anchor.sapling_final_state_bytes = vec![0, 0, 0];
    rebuilt_anchor.orchard_final_state_bytes = vec![0, 0, 0];
    rebuilt_anchor.ironwood_final_state_bytes = vec![0, 0, 0];
    let rebuilt_id = storage.recreate_with_account(&seed, rebuilt_anchor).await?;

    assert_eq!(
        storage.find_visible_tip().await?,
        None,
        "the observed tip is derived state and must not survive a rebuild"
    );
    assert_eq!(
        storage.find_idempotent_submission(&key).await?,
        None,
        "send idempotency records live in the discarded ledger"
    );
    assert!(
        storage.list_active_holds(rebuilt_id).await?.is_empty(),
        "holds live in the discarded ledger"
    );
    assert_eq!(
        storage.account_birthday().await?,
        BlockHeight::from(10),
        "the rebuilt account must be anchored at the new birthday"
    );
    Ok(())
}

#[tokio::test]
async fn account_birthday_requires_account() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = open_storage(&temp);
    storage.open_or_create().await?;

    let outcome = storage.account_birthday().await;
    assert!(
        matches!(outcome, Err(StorageError::AccountNotFound)),
        "a wallet with no account has no birthday: {outcome:?}"
    );

    let seed = fresh_seed();
    storage
        .create_account_for_seed(&seed, genesis_chain_state())
        .await?;
    assert_eq!(storage.account_birthday().await?, BlockHeight::from(1));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("idempotency key error: {0}")]
    Key(#[from] IdempotencyKeyError),
    #[error("zatoshis error: {0}")]
    Zatoshis(#[from] ZatoshisError),
}
