# Proposal 0001: Wallet read surface for downstream auto-shield consumers

| Field            | Value                                                                                                                                                       |
| ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status           | Proposed                                                                                                                                                    |
| Product          | Zally                                                                                                                                                       |
| Domain           | `zally-wallet` public API, `zally-storage` read surface                                                                                                     |
| Consumer        | [fauzec](https://github.com/ZcashFoundation/fauzec) (testnet faucet)                                                                                        |
| Pinned at        | `zally-wallet` rev `ba15aa307bd6a2cbabab6e0c0ad5d0451dfed779`                                                                                               |
| Related          | [Public interfaces](../architecture/public-interfaces.md), [ADR-0001 crate boundaries](../adrs/0001-workspace-crate-boundaries.md)                          |

## Context

Fauzec is the first non-test consumer to run `Wallet::shield_transparent_funds` on a cadence (see fauzec ADR-0006: auto-shield mining rewards). The faucet's funding gate reads "what can I pay out right now?" before every claim, and an operator-facing diagnostics surface reports per-pool breakdowns to humans and agents. Building those two surfaces against the current `Wallet` API forced two compromises that erode the typed-boundary discipline we want at the consumer layer:

1. **Per-pool balance is not a first-class read.** `Wallet::list_unspent_shielded_notes` returns the full note set, leaving the caller to sum and partition by `ShieldedPool`. Transparent value has no public read at all: `WalletStorage::list_transparent_receivers` returns scriptPubKeys without amounts, and `ChainSource::transparent_utxos` is documented as a `Wallet::sync` internal (SYNC-9 in the spine). Consumers either skip transparent reporting, scrape Zebra RPC directly, or wait for a `NothingToShield` cycle to learn the available transparent value.
2. **Reading the wallet's current addresses requires deriving a new one.** `Wallet::derive_next_address` and `Wallet::derive_next_address_with_transparent` are the only public surface that returns a Unified Address. Both walk forward through diversifier indices and, for the transparent flavour, burn one of the BIP-44 10-address gap-limit slots. A diagnostics endpoint that wanted to print "this wallet's miner address is `tm...`" cannot do so safely; it can only allocate a fresh address.

Both gaps push consumers toward workarounds that drift away from Zally's typed-boundary discipline. Auto-shielding is the precedent here: the gap that broke fauzec's funding loop in the field will reappear for the next custody integrator that wants periodic shielding plus operator-facing balance reporting.

## Goals

- Make per-pool wallet balance a first-class typed read on `Wallet`, with the same `Network` discipline as the rest of the API.
- Make wallet-owned Unified Addresses queryable without advancing diversifier indices or burning gap-limit slots.
- Keep both additions read-only; no new spending surface, no change to `WalletPlane`-shaped writes.

## Non-goals

- No change to `shield_transparent_funds` semantics, idempotency, or the `ShieldTransparentPlan` shape.
- No change to `WalletStorage::list_transparent_receivers` or `ChainSource::transparent_utxos` shapes; the new methods compose existing storage reads, they do not redefine them.
- No new spending or signing path. The proposal is strictly observational.
- No change to confirmations policy. Transparent maturity stays "what the chain source reports as unspent at the current `visible_tip`."

## Proposed API additions

### `Wallet::get_account_balance(account_id) -> AccountBalance`

```rust
impl Wallet {
    /// Returns the per-pool balance for `account_id`, anchored to the wallet's
    /// persisted visible tip.
    ///
    /// `not_retryable` on unknown account; `retryable` on transient storage I/O.
    pub async fn get_account_balance(
        &self,
        account_id: AccountId,
    ) -> Result<AccountBalance, WalletError>;
}
```

Returned type, network-tagged per the spine rule:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct AccountBalance {
    pub network: Network,
    pub sapling_zat: Zatoshis,
    pub orchard_zat: Zatoshis,
    pub transparent_mature_zat: Zatoshis,
    pub transparent_immature_zat: Zatoshis,
    /// Persisted visible tip the values are computed against. `None` when the
    /// wallet has not yet recorded a tip.
    pub as_of_height: Option<BlockHeight>,
}

impl AccountBalance {
    /// Sum of Sapling and Orchard balances. Spending-pool view.
    pub const fn shielded_zat(&self) -> Zatoshis;

    /// Sum of mature and immature transparent balances. Reporting view.
    pub const fn transparent_zat(&self) -> Zatoshis;

    pub const fn total_zat(&self) -> Zatoshis;
}
```

Naming follows the spine: `_zat` for integer zatoshis, `_height` for the tip anchor, `get_` for a cached read (the values are computed from the same `WalletStorage` rows that drive `list_unspent_shielded_notes`).

The mature/immature split for transparent value uses the same height arithmetic Zally already runs internally: `mature` are UTXOs with `mined_height + 100 <= visible_tip`; the remainder is `immature`. Coinbase maturity remains a chain-source property, not a Zally gate. The 100-block constant is the only Zcash-protocol constant baked into the method.

### `Wallet::list_exposed_addresses(account_id) -> Vec<ExposedAddress>`

```rust
impl Wallet {
    /// Lists every Unified Address previously exposed for `account_id`,
    /// without deriving a new one. Returned in derivation order.
    ///
    /// `not_retryable` on unknown account; `retryable` on transient storage I/O.
    pub async fn list_exposed_addresses(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<ExposedAddress>, WalletError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ExposedAddress {
    pub network: Network,
    pub unified_address: UnifiedAddress,
    pub diversifier_index: DiversifierIndex,
    /// Whether this UA carries a P2PKH (transparent) receiver. False for
    /// shielded-only UAs returned by `derive_next_address`.
    pub has_transparent_receiver: bool,
    /// Block height the wallet first observed the address as exposed, when
    /// recorded by the storage layer. `None` for addresses derived offline.
    pub exposed_at_height: Option<BlockHeight>,
}
```

`list_exposed_addresses` is a read-only counterpart to `derive_next_address` / `derive_next_address_with_transparent`. It calls into the existing `WalletStorage::list_addresses` surface (which `derive_next_address` already uses internally to compute the next diversifier) and returns the diversifier-ordered set. No state changes, no gap-limit progress, no new transparent receivers reserved.

The `has_transparent_receiver` flag lets a custody dashboard answer "which of these UAs has the receiver that Zebra was told to mine to?" without parsing the UA encoding.

## Why the consumer can't paper over these gaps

Fauzec considered four workarounds before requesting this PRD:

- **Compute balance in the consumer.** Fauzec already does this for the shielded breakdown (`AccountBalance::shielded_zat = sapling + orchard`). It cannot do it for the transparent breakdown because `WalletStorage::list_transparent_receivers` returns scriptPubKeys, not UTXO amounts; the amounts live behind storage methods that are not surfaced through `Wallet`.
- **Scrape Zebra RPC for transparent balance.** Works for testnet but couples the consumer to a second RPC client, fights the Zally-as-single-wallet-boundary stance from ADR-0001, and re-introduces the kind of cross-source vocabulary drift the spine is designed to prevent.
- **Trigger `shield_transparent_funds` and read `ShieldOutcome::NothingToShield::available_transparent_zat`.** Works but conflates a read with a side-effecting write attempt. It also reports nothing about the immature transparent backlog the operator needs to size the shielding threshold against.
- **Derive a fresh address with `derive_next_address_with_transparent` and compare against the configured miner address.** Burns one of the BIP-44 gap-limit slots per diagnostics read. Unusable for periodic polling.

Both proposed methods compose the storage surface that already exists; they are not new database access patterns. They surface the data that `Wallet::sync` and `Wallet::derive_next_address` already touch internally.

## Acceptance criteria

A change shipping under this proposal must:

1. Add `Wallet::get_account_balance` and `AccountBalance` to `zally-wallet`, with the public-interfaces spine entry, a rustdoc example, and at least one T1 integration test that exercises an account with sapling, orchard, and transparent UTXOs and asserts each pool field.
2. Add `Wallet::list_exposed_addresses` and `ExposedAddress` to `zally-wallet`, with a T1 test that confirms two calls return identical results without changing `derive_next_address` output (gap-limit invariant).
3. Pass the standard validation gate (`cargo fmt --check`, `cargo check --workspace --all-targets --all-features`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo nextest run --profile=ci`, rustdoc with `-D warnings`, `cargo deny`, `cargo machete`).
4. Pass the live `ci-live` profile against z3 regtest with `Wallet::sync` then `get_account_balance` returning consistent values across two consecutive scans.
5. Be additive: existing methods (`list_unspent_shielded_notes`, `derive_next_address`, `derive_next_address_with_transparent`, `shield_transparent_funds`) keep their current shape and semantics.
6. Update the public-interfaces spine with the two new verbs and types under the existing `get_*` / `list_*` rows, and (if the change touches the storage trait) update the SYNC-* / SPEND-* invariant numbering.

## Open questions

- Should `AccountBalance` carry per-pool note counts as well (e.g. `sapling_note_count`)? Fauzec does not need them for the funding gate but a custody dashboard might. Default position: omit, add later if a consumer asks.
- Should `list_exposed_addresses` paginate? A long-running custody account could accumulate thousands of UAs. Default position: return all in derivation order; revisit if any consumer reports memory pressure.
- Should `ExposedAddress::exposed_at_height` be required (`BlockHeight`) instead of optional? Depends on whether the storage backend can backfill heights for addresses derived offline; if not, optional is the honest shape.
- Does the transparent maturity split belong on `AccountBalance` or on a separate `TransparentBalance` view? The split is small enough that one struct stays readable, and consumers reading mature vs immature for ops alerts are the same consumers reading sapling vs orchard for the funding gate.

## Downstream impact

- **fauzec**: removes the `as_of_height: None` placeholder on `AccountBalance` and surfaces `transparent_mature_zat` in the `/diagnostics` payload alongside the existing shielded fields. Eliminates the deferred "wallet-describe" follow-up by replacing it with a `list_exposed_addresses(...)` call that produces the same data without burning a gap-limit slot.
- **Other custody integrators**: the same two reads cover the operator-dashboard use case (per-pool balance reporting) and the seed-rotation verification use case (which UA does my wallet currently own).
- **Signer-only services**: no impact. Both additions are on `zally-wallet`, which signer-only paths do not depend on per ADR-0001.
