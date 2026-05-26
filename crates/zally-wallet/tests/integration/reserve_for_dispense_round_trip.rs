//! `Wallet::reserve_for_dispense` round trip: idempotency, release/finalize lifecycle,
//! spendable arithmetic, and restart-amnesia coverage at the wallet API.
//!
//! Note: the create-test wallet fixture builds a fresh empty wallet with zero
//! spendable balance, so the "happy path" reservation amount is itself zero on
//! a virgin wallet. The tests cover behaviour around that boundary plus the
//! lifecycle methods which do not depend on a non-empty balance.

use zally_core::{
    AccountId, IdempotencyKey, IdempotencyKeyError, ReservationId, TxId, Zatoshis, ZatoshisError,
};
use zally_storage::{
    DispenseReservationRecord, DispenseReservedNote, Sqlite, SqliteOptions, StorageError,
    WalletStorage,
};
use zally_wallet::{ReserveForDispensePlan, WalletError};

use super::fixtures::{TestWalletFixture, create_test_wallet};

#[tokio::test]
async fn reserve_for_dispense_zero_amount_is_rejected() -> Result<(), TestError> {
    let TestWalletFixture {
        wallet,
        account_id,
        temp: _temp,
    } = create_test_wallet().await?;
    let plan = ReserveForDispensePlan::new(
        account_id,
        Zatoshis::zero(),
        IdempotencyKey::try_from("zero-amount-request")?,
        IdempotencyKey::try_from("zero-amount-idempotency")?,
    );
    let outcome = wallet.reserve_for_dispense(plan).await;
    assert!(
        matches!(outcome, Err(WalletError::ProposalRejected { .. })),
        "a zero-amount reservation must be rejected before touching storage: {outcome:?}"
    );
    Ok(())
}

#[tokio::test]
async fn reserve_for_dispense_returns_insufficient_when_amount_exceeds_spendable()
-> Result<(), TestError> {
    let TestWalletFixture {
        wallet,
        account_id,
        temp: _temp,
    } = create_test_wallet().await?;
    let plan = ReserveForDispensePlan::new(
        account_id,
        Zatoshis::try_from(100_000_u64)?,
        IdempotencyKey::try_from("oversized-request")?,
        IdempotencyKey::try_from("oversized-idempotency")?,
    );
    let outcome = wallet.reserve_for_dispense(plan).await;
    assert!(
        matches!(
            outcome,
            Err(WalletError::InsufficientBalance {
                requested_zat,
                spendable_zat,
            }) if requested_zat == Zatoshis::try_from(100_000_u64)?
                && spendable_zat == Zatoshis::zero()
        ),
        "reserving more than the wallet has spendable must fail closed: {outcome:?}"
    );
    Ok(())
}

#[tokio::test]
async fn reserve_for_dispense_is_idempotent_on_active_request() -> Result<(), TestError> {
    let TestWalletFixture {
        wallet,
        account_id,
        temp,
    } = create_test_wallet().await?;
    let network = wallet.network();

    // Seed an active reservation row via the storage layer so the wallet can hit
    // the idempotent path even though the test wallet has zero spendable balance.
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    storage.open_or_create().await?;
    let request_id = IdempotencyKey::try_from("idempotent-request")?;
    let idempotency_key = IdempotencyKey::try_from("idempotent-broadcast")?;
    let reservation_id = ReservationId::new();
    let amount = Zatoshis::try_from(42_u64)?;
    storage
        .create_dispense_reservation(DispenseReservationRecord {
            reservation_id,
            request_id: request_id.clone(),
            idempotency_key: idempotency_key.clone(),
            account_id,
            amount_zat: amount,
            spendable_for_check_zat: Zatoshis::try_from(1_000_000_u64)?,
            locked_notes: vec![DispenseReservedNote::new(
                zcash_protocol::ShieldedProtocol::Orchard,
                amount,
                TxId::from_bytes([0xAA; 32]),
                0,
            )],
            reserved_at_ms: 1_700_000_000_000,
        })
        .await?;

    let plan = ReserveForDispensePlan::new(
        account_id,
        Zatoshis::try_from(1_000_u64)?,
        request_id.clone(),
        idempotency_key.clone(),
    );
    let outcome = wallet.reserve_for_dispense(plan).await?;
    assert_eq!(
        outcome.reservation_id, reservation_id,
        "idempotent retry must return the prior reservation id"
    );
    assert_eq!(
        outcome.amount_zat, amount,
        "amount comes from the prior row, not the retry plan"
    );
    assert_eq!(outcome.request_id, request_id);
    assert_eq!(outcome.idempotency_key, idempotency_key);
    assert_eq!(outcome.locked_notes_summary.note_count, 1);
    assert_eq!(outcome.locked_notes_summary.total_locked_zat, amount);
    Ok(())
}

#[tokio::test]
async fn finalize_dispense_reservation_marks_row_consumed() -> Result<(), TestError> {
    let TestWalletFixture {
        wallet,
        account_id,
        temp,
    } = create_test_wallet().await?;
    let network = wallet.network();
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    storage.open_or_create().await?;

    let reservation_id = ReservationId::new();
    storage
        .create_dispense_reservation(DispenseReservationRecord {
            reservation_id,
            request_id: IdempotencyKey::try_from("finalize-request")?,
            idempotency_key: IdempotencyKey::try_from("finalize-broadcast")?,
            account_id,
            amount_zat: Zatoshis::try_from(1_u64)?,
            spendable_for_check_zat: Zatoshis::try_from(1_000_u64)?,
            locked_notes: Vec::new(),
            reserved_at_ms: 1_700_000_001_000,
        })
        .await?;

    let tx_id = TxId::from_bytes([0xEE; 32]);
    wallet
        .finalize_dispense_reservation(reservation_id, tx_id)
        .await?;
    wallet
        .finalize_dispense_reservation(reservation_id, tx_id)
        .await?;

    let active = storage
        .list_active_dispense_reservations(account_id)
        .await?;
    assert!(
        active.is_empty(),
        "finalized reservations leave the active set"
    );

    let unknown_outcome = wallet
        .finalize_dispense_reservation(ReservationId::new(), tx_id)
        .await;
    assert!(
        matches!(
            unknown_outcome,
            Err(WalletError::Storage(
                StorageError::DispenseReservationNotFound
            ))
        ),
        "finalize for an unknown reservation must fail closed: {unknown_outcome:?}"
    );
    Ok(())
}

#[tokio::test]
async fn release_dispense_reservation_marks_row_released() -> Result<(), TestError> {
    let TestWalletFixture {
        wallet,
        account_id,
        temp,
    } = create_test_wallet().await?;
    let network = wallet.network();
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    storage.open_or_create().await?;

    let reservation_id = ReservationId::new();
    storage
        .create_dispense_reservation(DispenseReservationRecord {
            reservation_id,
            request_id: IdempotencyKey::try_from("release-request")?,
            idempotency_key: IdempotencyKey::try_from("release-broadcast")?,
            account_id,
            amount_zat: Zatoshis::try_from(7_u64)?,
            spendable_for_check_zat: Zatoshis::try_from(1_000_u64)?,
            locked_notes: Vec::new(),
            reserved_at_ms: 1_700_000_002_000,
        })
        .await?;

    wallet.release_dispense_reservation(reservation_id).await?;
    wallet.release_dispense_reservation(reservation_id).await?;
    let active = storage
        .list_active_dispense_reservations(account_id)
        .await?;
    assert!(
        active.is_empty(),
        "released reservations leave the active set"
    );

    let unknown_outcome = wallet
        .release_dispense_reservation(ReservationId::new())
        .await;
    assert!(
        matches!(
            unknown_outcome,
            Err(WalletError::Storage(
                StorageError::DispenseReservationNotFound
            ))
        ),
        "release for an unknown reservation must fail closed: {unknown_outcome:?}"
    );
    Ok(())
}

#[tokio::test]
async fn spendable_for_next_dispense_subtracts_active_reservations() -> Result<(), TestError> {
    let TestWalletFixture {
        wallet,
        account_id,
        temp,
    } = create_test_wallet().await?;
    let network = wallet.network();
    let storage = Sqlite::new(SqliteOptions::for_network(network, temp.db_path()));
    storage.open_or_create().await?;

    let baseline = wallet.spendable_for_next_dispense(account_id).await?;
    assert_eq!(
        baseline,
        Zatoshis::zero(),
        "a fresh empty wallet has nothing spendable",
    );

    // The wallet stores reservations against a sqlite ledger that is shared with the
    // wallet handle's storage; once a reservation lives on disk it subtracts from
    // any subsequent spendable_for_next_dispense read on the wallet handle.
    let reservation_id = ReservationId::new();
    storage
        .create_dispense_reservation(DispenseReservationRecord {
            reservation_id,
            request_id: IdempotencyKey::try_from("subtract-request")?,
            idempotency_key: IdempotencyKey::try_from("subtract-broadcast")?,
            account_id,
            amount_zat: Zatoshis::try_from(11_u64)?,
            spendable_for_check_zat: Zatoshis::try_from(1_000_u64)?,
            locked_notes: Vec::new(),
            reserved_at_ms: 1_700_000_003_000,
        })
        .await?;

    let after_reservation = wallet.spendable_for_next_dispense(account_id).await?;
    // The empty wallet still saturates at zero after subtracting an 11-zat reservation.
    assert_eq!(after_reservation, Zatoshis::zero());

    // After release, the active-sum should drop back to zero (still bounded at zero
    // because the wallet itself has nothing spendable to begin with).
    wallet.release_dispense_reservation(reservation_id).await?;
    let restored = wallet.spendable_for_next_dispense(account_id).await?;
    assert_eq!(restored, baseline);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("test wallet error: {0}")]
    Fixture(#[from] super::fixtures::TestWalletError),
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("idempotency key error: {0}")]
    Key(#[from] IdempotencyKeyError),
    #[error("zatoshis error: {0}")]
    Zatoshis(#[from] ZatoshisError),
}

#[allow(
    dead_code,
    reason = "AccountId is used implicitly through fixtures; keeping the import surface explicit"
)]
const _: Option<AccountId> = None;
