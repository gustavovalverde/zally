# ADR-0003: Dispense Reservations

| Field   | Value                                                                                                                                                                                                                                                              |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status  | Accepted on 2026-05-26                                                                                                                                                                                                                                             |
| Product | Zally                                                                                                                                                                                                                                                              |
| Domain  | Wallet plane, storage migrations, dispense workflows                                                                                                                                                                                                               |
| Related | [Public interfaces](../architecture/public-interfaces.md); [ADR-0001 Workspace crate boundaries](./0001-workspace-crate-boundaries.md); [fauzec dispense-plane redesign plan](https://github.com/gustavovalverde/fauzec/blob/main/docs/plans/2026-05-25-dispense-plane-redesign.md) Phase 9 |

## Context

Faucet-style and exchange-style operators repeatedly serialise a "reserve funds, then build a transaction" sequence at the application layer because Zally previously offered no atomic primitive for it. The fauzec faucet keeps an in-memory earmark map, optimistically subtracts amounts from `Wallet::list_unspent_shielded_notes`, and then calls `Wallet::send_payment`. Two failures fall out of that shape:

- The earmark map and librustzcash's `GreedyInputSelector` disagree about "spendable." Two claims that race within the post-broadcast, pre-confirmation window can both pass the earmark precondition, and the second `send_payment` then fails inside `create_proposed_transactions` with insufficient balance even though the wallet has the funds; they are simply already committed.
- The earmark map is in-memory and is not reconstructed at startup. A process restart between earmark and broadcast forgets the lock, and a subsequent call selects the same notes again.

Both failures live one layer above Zally. The wallet layer already serialises every `WalletDb` access through a single-threaded actor (see `zally-storage::sqlite`) and already persists ancillary tables (`ext_zally_idempotency`, `ext_zally_pending_broadcast_inputs`, `ext_zally_observed_tip`) alongside the librustzcash schema. The atomic-reservation operation belongs here.

## Decision

1. **Reservations are a first-class wallet operation.** `Wallet` gains four methods: `reserve_for_dispense`, `release_dispense_reservation`, `finalize_dispense_reservation`, and `spendable_for_next_dispense`. Each reservation row carries the caller-supplied request identifier (idempotency anchor for the reservation), the caller-supplied idempotency key for the eventual broadcast, the reserved amount, a snapshot of the notes selected at reservation time, and lifecycle timestamps. The wallet returns a typed `DispenseReservation` with the wallet-issued `ReservationId`, a `LockedNotesSummary`, and an `available_after_release_zat` projection.

2. **Reservations are amount locks, not note locks.** The reservation row records the notes selected at the moment the reservation was recorded, but the enforcement contract is amount-based: the sum of active reservations for an account must stay at or below the wallet's shielded-spendable view. Note-level locking would couple every reservation to librustzcash's input-selection internals; an amount lock composes with the existing pending-broadcast filter and with whatever input selector future librustzcash releases ship. The note set is recorded for operator visibility and for downstream observability.

3. **Atomic precondition + insert lives in storage.** `WalletStorage::create_dispense_reservation` runs the precondition recheck and the row insert inside one sqlite transaction. Two concurrent reservations whose amounts sum above spendable cannot both pass: one transaction observes the other's row before the second commits and returns `StorageError::InsufficientFunds`. The single-threaded wallet-db actor already serialises actor-level access, but the explicit sqlite transaction keeps the storage layer correct against a future multi-actor refactor.

4. **`request_id` is the reservation idempotency anchor; `idempotency_key` is the broadcast anchor.** A second `reserve_for_dispense` call with the same `request_id` whose prior reservation is still active returns the prior reservation unchanged. Once that reservation is finalized or released the `request_id` becomes reusable. The broadcast's `idempotency_key` is recorded on the row so the dispense path can pair the reservation with its idempotent submission ledger entry (`ext_zally_idempotency`) without a second lookup. Callers that prefer to keep both identifiers equal may do so; storage treats them as separate columns either way.

5. **Lifecycle methods are idempotent on second call.** `release_dispense_reservation` and `finalize_dispense_reservation` return `Ok(())` on a row that has already transitioned. Both fail with `DispenseReservationNotFound` for an unknown reservation. The wallet exposes the typed `WalletError` projection; storage exposes `StorageError::DispenseReservationNotFound`.

6. **`spendable_for_next_dispense` is the canonical "what can I reserve right now?" view.** It returns the wallet's shielded spendable balance (Sapling plus Orchard) minus the sum of every active reservation for the account. Zally has no auto-managed hot-reserve concept today; if one is introduced it subtracts from this view as well.

7. **The storage migration is additive.** A new `ext_zally_dispense_reservations` table is created at `WalletStorage::open_or_create` time, alongside the existing `ext_zally_*` tables. A unique partial index `idx_dispense_reservations_active_request` enforces the active-row request-id invariant in the database, not just in Rust. A non-unique `idx_dispense_reservations_account_active` indexes the active-sum query path.

8. **The `locked_notes` blob uses a compact binary encoding, not JSON.** Each reserved note is 45 bytes (1-byte protocol tag, 8-byte big-endian zatoshi count, 32-byte `tx_id`, 4-byte big-endian output index), prefixed by a 4-byte big-endian count. The encoding is internal to `zally-storage`; it stays out of any public Zally type. Protocol tags `0` and `1` are stable across releases; an additional pool gets the next free tag value with a migration step that rewrites prior rows only if their semantics change.

## Consequences

- Applications stop owning their own earmark store. The wallet plane is the single authority for "this amount is committed to a future dispense"; the application layer holds nothing the wallet does not already persist.
- Reservation survives process restart. `WalletStorage::list_active_dispense_reservations` returns every row that is neither finalized nor released, so a restarted dispense pipeline can hydrate its in-flight set from the wallet rather than rebuilding it from observability sources.
- Two concurrent reservations whose amounts sum above spendable get a typed, observable refusal: one transaction commits, the other returns `InsufficientFunds`. The application layer does not need to retry-and-recheck.
- The wallet exposes the same `InsufficientBalance` variant for reserve-time and propose-time refusals; downstream code that already handles `WalletError::InsufficientBalance` stays unchanged.
- The migration adds three indices to the `ext_zally_*` schema. Re-opening an existing wallet file applies the migration idempotently; there is no schema number to bump because `ext_zally_*` tables are created with `IF NOT EXISTS`.

## Alternatives considered

- **Note-level locking.** Rejected: librustzcash does not expose an input-selection hook keyed on a wallet-owned "do not pick" set for shielded notes the way it does for transparent outpoints (`FilteredWalletDb::get_spendable_transparent_outputs`). A shielded note-level lock would require either patching librustzcash or building a parallel proposal pipeline that bypasses `GreedyInputSelector`. Amount locking gives the same observable guarantee at the application layer (two concurrent reservations cannot both pass) without coupling to internal selector behaviour.
- **Persist reservations as a column on `ext_zally_idempotency`.** Rejected: idempotency rows are keyed by the broadcast's idempotency key and exist only once the broadcast succeeded. Reservations exist before broadcast, can outlive the call that created them (because the reserver can crash between reserve and broadcast), and need a separate request-id idempotency anchor. Forcing them onto the existing table would conflate two distinct lifecycles.
- **Serialize `locked_notes` as JSON via `serde_json`.** Rejected: it would add a new direct dependency to `zally-storage` for purely-informational data. A 45-byte-per-note fixed layout keeps the storage crate's dependency graph unchanged and round-trips losslessly.
