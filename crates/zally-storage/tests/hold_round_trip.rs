//! `WalletStorage` dispense-reservation lifecycle and crash-recovery contract.
//!
//! Covers the storage-side guarantees Phase 9 of the fauzec dispense-plane
//! redesign asks for: amount-sum enforcement under concurrent reservation,
//! idempotent release/finalize, request-id conflict, and survival of an
//! open/close round trip (the equivalent of a process restart at storage
//! granularity).

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;
use uuid::Uuid;
use zally_core::{
    AccountId, HoldId, IdempotencyKey, IdempotencyKeyError, Network, TxId, Zatoshis, ZatoshisError,
};
use zally_storage::{HeldNote, HoldRecord, Sqlite, SqliteOptions, StorageError, WalletStorage};

fn one_zec_in_zats() -> Result<Zatoshis, ZatoshisError> {
    Zatoshis::try_from(100_000_000_u64)
}

fn make_account() -> AccountId {
    AccountId::from_uuid(Uuid::new_v4())
}

fn make_request_id(suffix: u32) -> Result<IdempotencyKey, IdempotencyKeyError> {
    IdempotencyKey::try_from(format!("request-{suffix:08x}"))
}

fn open_storage(db_path: PathBuf) -> Sqlite {
    Sqlite::new(SqliteOptions::for_network(Network::regtest(), db_path))
}

struct HoldFixture {
    hold_id: HoldId,
    request_id: IdempotencyKey,
    idempotency_key: IdempotencyKey,
    account_id: AccountId,
    amount_zat: Zatoshis,
    spendable_for_check_zat: Zatoshis,
    reserved_at_ms: u64,
}

fn make_record(fixture: HoldFixture) -> HoldRecord {
    let HoldFixture {
        hold_id,
        request_id,
        idempotency_key,
        account_id,
        amount_zat,
        spendable_for_check_zat,
        reserved_at_ms,
    } = fixture;
    HoldRecord {
        hold_id,
        request_id,
        idempotency_key,
        account_id,
        amount_zat,
        spendable_for_check_zat,
        locked_notes: vec![HeldNote::new(
            zcash_protocol::ShieldedPool::Orchard,
            amount_zat,
            TxId::from_bytes([0xAB; 32]),
            0,
        )],
        reserved_at_ms,
    }
}

#[tokio::test]
async fn hold_round_trip_persists_active_row() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = open_storage(temp.path().join("wallet.db"));
    storage.open_or_create().await?;

    let account = make_account();
    let request_id = make_request_id(0)?;
    let idempotency_key = IdempotencyKey::try_from("idempotency-00000000")?;
    let amount = Zatoshis::try_from(75_000_000_u64)?;
    let hold_id = HoldId::new();

    storage
        .create_hold(make_record(HoldFixture {
            hold_id,
            request_id: request_id.clone(),
            idempotency_key: idempotency_key.clone(),
            account_id: account,
            amount_zat: amount,
            spendable_for_check_zat: one_zec_in_zats()?,
            reserved_at_ms: 1_700_000_000_000,
        }))
        .await?;

    let active = storage.list_active_holds(account).await?;
    assert_eq!(active.len(), 1, "one active reservation should be visible");
    assert_eq!(active[0].hold_id, hold_id);
    assert_eq!(active[0].amount_zat, amount);
    assert!(active[0].is_active());
    assert_eq!(active[0].locked_notes.len(), 1);
    let locked = active[0].locked_notes[0];
    assert_eq!(locked.protocol, zcash_protocol::ShieldedPool::Orchard);
    assert_eq!(locked.value_zat, amount);
    assert_eq!(locked.tx_id, TxId::from_bytes([0xAB; 32]));

    let sum = storage.sum_active_dispense_reserved_zat(account).await?;
    assert_eq!(sum, amount);

    let by_request = storage
        .find_hold_by_request_id(&request_id)
        .await?
        .ok_or(TestError::MissingByRequest)?;
    assert_eq!(by_request.hold_id, hold_id);
    assert_eq!(by_request.idempotency_key, idempotency_key);

    Ok(())
}

#[tokio::test]
async fn holds_rejects_oversubscription() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = open_storage(temp.path().join("wallet.db"));
    storage.open_or_create().await?;

    let account = make_account();
    let spendable = one_zec_in_zats()?;
    let amount_each = Zatoshis::try_from(60_000_000_u64)?;

    storage
        .create_hold(make_record(HoldFixture {
            hold_id: HoldId::new(),
            request_id: make_request_id(1)?,
            idempotency_key: IdempotencyKey::try_from("idempotency-00000001")?,
            account_id: account,
            amount_zat: amount_each,
            spendable_for_check_zat: spendable,
            reserved_at_ms: 1_700_000_001_000,
        }))
        .await?;

    let outcome = storage
        .create_hold(make_record(HoldFixture {
            hold_id: HoldId::new(),
            request_id: make_request_id(2)?,
            idempotency_key: IdempotencyKey::try_from("idempotency-00000002")?,
            account_id: account,
            amount_zat: amount_each,
            spendable_for_check_zat: spendable,
            reserved_at_ms: 1_700_000_002_000,
        }))
        .await;
    let expected_available = Zatoshis::try_from(spendable.as_u64() - amount_each.as_u64())?;
    assert!(
        matches!(
            outcome,
            Err(StorageError::InsufficientFunds {
                required_zat,
                available_zat,
            }) if required_zat == amount_each && available_zat == expected_available
        ),
        "second reservation must fail closed when sum would exceed spendable: {outcome:?}"
    );

    let sum = storage.sum_active_dispense_reserved_zat(account).await?;
    assert_eq!(
        sum, amount_each,
        "the rejected second reservation must leave the active sum unchanged"
    );
    Ok(())
}

#[tokio::test]
async fn holds_admit_disjoint_concurrent_callers() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Arc::new(open_storage(temp.path().join("wallet.db")));
    storage.open_or_create().await?;

    let account = make_account();
    let spendable = one_zec_in_zats()?;
    let amount_a = Zatoshis::try_from(40_000_000_u64)?;
    let amount_b = Zatoshis::try_from(50_000_000_u64)?;

    let storage_a = Arc::clone(&storage);
    let storage_b = Arc::clone(&storage);
    let record_a = make_record(HoldFixture {
        hold_id: HoldId::new(),
        request_id: make_request_id(10)?,
        idempotency_key: IdempotencyKey::try_from("idempotency-0000000a")?,
        account_id: account,
        amount_zat: amount_a,
        spendable_for_check_zat: spendable,
        reserved_at_ms: 1_700_000_010_000,
    });
    let record_b = make_record(HoldFixture {
        hold_id: HoldId::new(),
        request_id: make_request_id(11)?,
        idempotency_key: IdempotencyKey::try_from("idempotency-0000000b")?,
        account_id: account,
        amount_zat: amount_b,
        spendable_for_check_zat: spendable,
        reserved_at_ms: 1_700_000_011_000,
    });

    let (left, right) = tokio::join!(
        async move { storage_a.create_hold(record_a).await },
        async move { storage_b.create_hold(record_b).await },
    );
    left?;
    right?;

    let active = storage.list_active_holds(account).await?;
    assert_eq!(active.len(), 2, "both concurrent reservations must persist");
    let sum = storage.sum_active_dispense_reserved_zat(account).await?;
    let expected = Zatoshis::try_from(amount_a.as_u64().saturating_add(amount_b.as_u64()))?;
    assert_eq!(sum, expected);
    Ok(())
}

#[tokio::test]
async fn holds_admit_one_when_concurrent_oversubscribe() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = Arc::new(open_storage(temp.path().join("wallet.db")));
    storage.open_or_create().await?;

    let account = make_account();
    let spendable = one_zec_in_zats()?;
    let amount_each = Zatoshis::try_from(70_000_000_u64)?;

    let storage_a = Arc::clone(&storage);
    let storage_b = Arc::clone(&storage);
    let record_a = make_record(HoldFixture {
        hold_id: HoldId::new(),
        request_id: make_request_id(20)?,
        idempotency_key: IdempotencyKey::try_from("idempotency-00000014")?,
        account_id: account,
        amount_zat: amount_each,
        spendable_for_check_zat: spendable,
        reserved_at_ms: 1_700_000_020_000,
    });
    let record_b = make_record(HoldFixture {
        hold_id: HoldId::new(),
        request_id: make_request_id(21)?,
        idempotency_key: IdempotencyKey::try_from("idempotency-00000015")?,
        account_id: account,
        amount_zat: amount_each,
        spendable_for_check_zat: spendable,
        reserved_at_ms: 1_700_000_021_000,
    });

    let (left, right) = tokio::join!(
        async move { storage_a.create_hold(record_a).await },
        async move { storage_b.create_hold(record_b).await },
    );

    let outcomes = vec![left, right];
    let success_count = outcomes.iter().filter(|o| o.is_ok()).count();
    let insufficient_count = outcomes
        .iter()
        .filter(|o| matches!(o, Err(StorageError::InsufficientFunds { .. })))
        .count();
    assert_eq!(
        (success_count, insufficient_count),
        (1, 1),
        "exactly one concurrent reservation must succeed; the other must fail with InsufficientFunds: {outcomes:?}"
    );

    let sum = storage.sum_active_dispense_reserved_zat(account).await?;
    assert_eq!(
        sum, amount_each,
        "only the winning reservation must contribute to the active sum"
    );
    Ok(())
}

#[tokio::test]
async fn holds_release_is_idempotent() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = open_storage(temp.path().join("wallet.db"));
    storage.open_or_create().await?;

    let account = make_account();
    let hold_id = HoldId::new();
    let amount = Zatoshis::try_from(20_000_000_u64)?;
    storage
        .create_hold(make_record(HoldFixture {
            hold_id,
            request_id: make_request_id(30)?,
            idempotency_key: IdempotencyKey::try_from("idempotency-0000001e")?,
            account_id: account,
            amount_zat: amount,
            spendable_for_check_zat: one_zec_in_zats()?,
            reserved_at_ms: 1_700_000_030_000,
        }))
        .await?;

    storage.release_hold(hold_id, 1_700_000_040_000).await?;
    storage.release_hold(hold_id, 1_700_000_050_000).await?;

    let active = storage.list_active_holds(account).await?;
    assert!(
        active.is_empty(),
        "released reservation must drop out of the active list"
    );

    let missing = storage.release_hold(HoldId::new(), 1_700_000_060_000).await;
    assert!(
        matches!(missing, Err(StorageError::HoldNotFound)),
        "release of a missing reservation must fail closed: {missing:?}"
    );

    Ok(())
}

#[tokio::test]
async fn holds_finalize_is_idempotent() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = open_storage(temp.path().join("wallet.db"));
    storage.open_or_create().await?;

    let account = make_account();
    let hold_id = HoldId::new();
    let amount = Zatoshis::try_from(20_000_000_u64)?;
    storage
        .create_hold(make_record(HoldFixture {
            hold_id,
            request_id: make_request_id(40)?,
            idempotency_key: IdempotencyKey::try_from("idempotency-00000028")?,
            account_id: account,
            amount_zat: amount,
            spendable_for_check_zat: one_zec_in_zats()?,
            reserved_at_ms: 1_700_000_070_000,
        }))
        .await?;

    let tx_id = TxId::from_bytes([0xCC; 32]);
    storage.finalize_hold(hold_id, tx_id).await?;
    storage.finalize_hold(hold_id, tx_id).await?;

    let active = storage.list_active_holds(account).await?;
    assert!(
        active.is_empty(),
        "finalized reservation must drop out of the active list"
    );

    let missing = storage.finalize_hold(HoldId::new(), tx_id).await;
    assert!(
        matches!(missing, Err(StorageError::HoldNotFound)),
        "finalize of a missing reservation must fail closed: {missing:?}"
    );

    Ok(())
}

#[tokio::test]
async fn holds_reject_duplicate_active_request_id() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let storage = open_storage(temp.path().join("wallet.db"));
    storage.open_or_create().await?;

    let account = make_account();
    let request_id = make_request_id(50)?;
    let idempotency_key = IdempotencyKey::try_from("idempotency-00000032")?;
    let amount = Zatoshis::try_from(10_000_000_u64)?;
    storage
        .create_hold(make_record(HoldFixture {
            hold_id: HoldId::new(),
            request_id: request_id.clone(),
            idempotency_key: idempotency_key.clone(),
            account_id: account,
            amount_zat: amount,
            spendable_for_check_zat: one_zec_in_zats()?,
            reserved_at_ms: 1_700_000_080_000,
        }))
        .await?;

    let outcome = storage
        .create_hold(make_record(HoldFixture {
            hold_id: HoldId::new(),
            request_id,
            idempotency_key,
            account_id: account,
            amount_zat: amount,
            spendable_for_check_zat: one_zec_in_zats()?,
            reserved_at_ms: 1_700_000_080_001,
        }))
        .await;
    assert!(
        matches!(outcome, Err(StorageError::HoldRequestConflict)),
        "duplicate active request id must fail closed: {outcome:?}"
    );
    Ok(())
}

#[tokio::test]
async fn holds_survive_storage_restart() -> Result<(), TestError> {
    let temp = TempDir::new()?;
    let db_path = temp.path().join("wallet.db");
    let storage_a = open_storage(db_path.clone());
    storage_a.open_or_create().await?;

    let account = make_account();
    let hold_id = HoldId::new();
    let amount = Zatoshis::try_from(30_000_000_u64)?;
    storage_a
        .create_hold(make_record(HoldFixture {
            hold_id,
            request_id: make_request_id(60)?,
            idempotency_key: IdempotencyKey::try_from("idempotency-0000003c")?,
            account_id: account,
            amount_zat: amount,
            spendable_for_check_zat: one_zec_in_zats()?,
            reserved_at_ms: 1_700_000_090_000,
        }))
        .await?;
    drop(storage_a);

    let storage_b = open_storage(db_path);
    storage_b.open_or_create().await?;
    let active = storage_b.list_active_holds(account).await?;
    assert_eq!(
        active.len(),
        1,
        "reservation must survive a process restart"
    );
    assert_eq!(active[0].hold_id, hold_id);
    assert_eq!(active[0].amount_zat, amount);
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
    #[error("expected to find reservation by request id, found none")]
    MissingByRequest,
}
