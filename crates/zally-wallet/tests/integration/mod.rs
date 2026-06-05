//! Integration tests.

#[allow(
    dead_code,
    reason = "fixture helpers are consumed test-by-test; not every test uses every helper"
)]
mod fixtures;

mod capabilities_reports;
mod circuit_breaker_opens_after_threshold;
mod create_then_create_returns_already_exists;
mod create_then_open_round_trip;
mod create_then_open_round_trip_in_memory;
mod derive_address_populates_transparent;
mod get_account_balance_round_trip;
mod list_exposed_addresses_round_trip;
mod list_unspent_shielded_notes_round_trip;
mod metrics_snapshot_round_trip;
mod network_mismatch_fails_closed;
mod observe_emits_scan_progress;
mod open_or_create_account_round_trip;
mod open_without_seal_returns_no_sealed_seed;
mod pending_transparent_inputs_round_trip;
mod propose_rejects_memo_on_transparent;
mod propose_rejects_network_mismatch;
mod reserve_hold_round_trip;
mod send_payment_short_circuits_on_known_idempotency_key;
mod sync_catches_up_to_tip;
mod sync_driver_follows_chain;
mod sync_network_mismatch;
mod sync_retries_retryable_chain_failures;
mod to_uri_round_trips_through_propose;
#[cfg(feature = "unsafe_plaintext_seed")]
mod unsafe_plaintext_seed_warns;
