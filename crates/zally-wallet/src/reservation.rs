//! Dispense reservation handle returned by [`Wallet::reserve_for_dispense`].
//!
//! [`Wallet::reserve_for_dispense`]: crate::Wallet::reserve_for_dispense

use zally_core::{IdempotencyKey, Network, ReservationId, Zatoshis};

/// Summary of what the wallet locked in one reservation call.
///
/// Carries enough detail for the caller to log the reservation (note count, total
/// reserved value) without round-tripping the full set of shielded notes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LockedNotesSummary {
    /// Number of shielded notes the wallet considered locked at reservation time.
    pub note_count: u32,
    /// Total value across those locked notes.
    pub total_locked_zat: Zatoshis,
}

impl LockedNotesSummary {
    /// Constructs a locked-notes summary.
    #[must_use]
    pub const fn new(note_count: u32, total_locked_zat: Zatoshis) -> Self {
        Self {
            note_count,
            total_locked_zat,
        }
    }
}

/// Outcome of a successful [`Wallet::reserve_for_dispense`] call.
///
/// The wallet has persisted a reservation row keyed by `reservation_id` and bound
/// it to the caller-supplied `request_id`. Any future call to
/// [`Wallet::spendable_for_next_dispense`] subtracts `amount_zat` from the wallet's
/// spendable balance until the reservation is finalized or released.
///
/// [`Wallet::reserve_for_dispense`]: crate::Wallet::reserve_for_dispense
/// [`Wallet::spendable_for_next_dispense`]: crate::Wallet::spendable_for_next_dispense
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DispenseReservation {
    /// Network the wallet is bound to.
    pub network: Network,
    /// Wallet-issued identifier for this reservation. Consume it via
    /// [`Wallet::finalize_dispense_reservation`] or
    /// [`Wallet::release_dispense_reservation`].
    ///
    /// [`Wallet::finalize_dispense_reservation`]: crate::Wallet::finalize_dispense_reservation
    /// [`Wallet::release_dispense_reservation`]: crate::Wallet::release_dispense_reservation
    pub reservation_id: ReservationId,
    /// Caller-supplied request identifier (idempotency anchor) the reservation is bound to.
    pub request_id: IdempotencyKey,
    /// Caller-supplied idempotency key for the eventual broadcast.
    pub idempotency_key: IdempotencyKey,
    /// Amount reserved.
    pub amount_zat: Zatoshis,
    /// Summary of the shielded notes the wallet considered locked at reservation time.
    pub locked_notes_summary: LockedNotesSummary,
    /// Spendable amount the wallet would report **if this reservation were released
    /// right now**. Equals the pre-reservation spendable view; useful for callers
    /// that want to surface "you locked X out of Y available" without a second
    /// round-trip to [`Wallet::spendable_for_next_dispense`].
    ///
    /// [`Wallet::spendable_for_next_dispense`]: crate::Wallet::spendable_for_next_dispense
    pub available_after_release_zat: Zatoshis,
}
