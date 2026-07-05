//! Persisted dispense-reservation row.
//!
//! Returned by
//! [`WalletStorage::list_active_holds`](crate::WalletStorage::list_active_holds)
//! and
//! [`WalletStorage::find_hold_by_request_id`](crate::WalletStorage::find_hold_by_request_id).

use zally_core::{AccountId, HoldId, IdempotencyKey, TxId, Zatoshis};

/// A snapshot of one shielded note that contributed to a reservation at the moment
/// the reservation was recorded.
///
/// Carries enough detail for operators to inspect what the wallet considered locked
/// without re-running input selection. The wallet plane uses these for the
/// `locked_notes_summary` it returns from `Wallet::reserve_for_dispense`; the storage
/// layer round-trips the rows through a JSON-encoded blob in `locked_notes`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct HeldNote {
    /// Pool the locked note lives on.
    pub protocol: zcash_protocol::ShieldedPool,
    /// Note value at reservation time.
    pub value_zat: Zatoshis,
    /// Transaction that produced the note.
    pub tx_id: TxId,
    /// Output index within the producing transaction (Sapling output index or
    /// Orchard action index, depending on `protocol`).
    pub output_index: u32,
}

impl HeldNote {
    /// Constructs a reserved-note record. New fields land as additive parameters on
    /// purpose-specific constructors rather than positional arguments here.
    #[must_use]
    pub const fn new(
        protocol: zcash_protocol::ShieldedPool,
        value_zat: Zatoshis,
        tx_id: TxId,
        output_index: u32,
    ) -> Self {
        Self {
            protocol,
            value_zat,
            tx_id,
            output_index,
        }
    }
}

/// One persisted dispense reservation row.
///
/// Rows live in the Zally-owned `ext_zally_holds` table. A row is
/// considered **active** while both `finalized_tx_id` and `released_at_ms` are `None`.
/// Releasing the reservation sets `released_at_ms`; finalizing sets `finalized_tx_id`.
/// Active reservations subtract from the wallet's `spendable_for_next_dispense`
/// snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct HoldRow {
    /// Wallet-issued identifier for this reservation.
    pub hold_id: HoldId,
    /// Caller-supplied request identifier (idempotency anchor) for the reservation.
    pub request_id: IdempotencyKey,
    /// Caller-supplied idempotency key for the eventual broadcast.
    pub idempotency_key: IdempotencyKey,
    /// Account the reservation belongs to.
    pub account_id: AccountId,
    /// Amount reserved.
    pub amount_zat: Zatoshis,
    /// Notes selected at reservation time. Informational; the enforcement contract
    /// is amount-based.
    pub locked_notes: Vec<HeldNote>,
    /// Unix milliseconds when the reservation was recorded.
    pub reserved_at_ms: u64,
    /// If finalized, the broadcast transaction the reservation was consumed by.
    pub finalized_tx_id: Option<TxId>,
    /// If released, the Unix milliseconds when the release was recorded.
    pub released_at_ms: Option<u64>,
}

impl HoldRow {
    /// True when the reservation is neither finalized nor released and so still subtracts
    /// from `spendable_for_next_dispense`.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.finalized_tx_id.is_none() && self.released_at_ms.is_none()
    }
}
