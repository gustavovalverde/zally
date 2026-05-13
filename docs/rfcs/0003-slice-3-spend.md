# RFC 0003: Slice 3 — Spend (propose, sign, send)

| Field | Value |
|---|---|
| Status | Accepted |
| Product | Zally |
| Slice | 3 |
| Related | [PRD-0001](../prd-0001-zally-headless-wallet-library.md), [ADR-0002](../adrs/0002-founding-implementation-patterns.md), [RFC-0002](0002-slice-2-chain-and-sync.md) |
| Created | 2026-05-12 |

## Summary

Slice 3 lands the operator-facing spend surface: ZIP-321 payment-request parsing, the `Proposal` value (built via `zcash_client_backend::data_api::wallet::propose_transfer`), in-process signing via the seed sealing held by `Wallet`, broadcast via the `Submitter` trait from Slice 2, idempotent send semantics keyed by a caller-supplied `IdempotencyKey`, and the `SendOutcome` / `Confirmation` split shape that ADR-0002 Decision 4 records. The slice enforces ZIP-302 (no memos on transparent recipients), ZIP-320 (no shielded inputs on TEX recipients), ZIP-317 (conventional fees by default), and ZIP-203 (transaction expiry) at the API boundary.

Slice 3 ships against `MockChainSource` + a new `MockSubmitter`. Real-chain end-to-end testing is gated by Slice 5's live-infrastructure plan. PCZT export (REQ-PCZT-1..5) ships in Slice 4, which depends on the proposal value defined here.

---

## 1. Public surface

### 1.1 New domain types in `zally-core`

```rust
/// Recipient of a payment.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum PaymentRecipient {
    /// Encoded Unified Address.
    UnifiedAddress { encoded: String, network: Network },
    /// Encoded Sapling address (legacy operator support).
    SaplingAddress { encoded: String, network: Network },
    /// Transparent P2PKH or P2SH address.
    TransparentAddress { encoded: String, network: Network },
    /// TEX address per ZIP-320. Refuses shielded inputs.
    TexAddress { encoded: String, network: Network },
}

/// Receiver-purpose vocabulary for the configurable-confirmation-depth API
/// (REQ-SYNC-4 + REQ-CORE-3 multi-receiver model).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ReceiverPurpose {
    /// Mining-pool coinbase receiver. Defaults to 100-block confirmation depth (ZIP-213).
    Mining,
    /// Donation receive.
    Donations,
    /// Hot-dispense receive (faucet payouts, exchange withdrawals).
    HotDispense,
    /// Cold-reserve receive.
    ColdReserve,
    /// Operator-defined purpose.
    Custom(String),
}
```

### 1.2 `zally-wallet` spend module

```rust
/// Fee strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FeeStrategy {
    /// ZIP-317 conventional fee. Default.
    Conventional,
    /// Custom flat fee. Operator opt-in.
    Custom { fee_zat: Zatoshis },
}

/// Parsed ZIP-321 payment request.
#[derive(Clone, Debug)]
pub struct PaymentRequest {
    /* private; wraps `zip321::TransactionRequest` */
}

impl PaymentRequest {
    pub fn from_uri(uri: &str, network: Network) -> Result<Self, PaymentRequestError>;
    pub fn payments(&self) -> &[ParsedPayment];
}

/// One payment in a `PaymentRequest`.
#[derive(Clone, Debug)]
pub struct ParsedPayment {
    pub recipient: PaymentRecipient,
    pub amount: Zatoshis,
    pub memo: Option<Memo>,
    pub label: Option<String>,
    pub message: Option<String>,
}

/// A spend proposal built by `Wallet::propose`. Not yet signed or submitted.
pub struct Proposal {
    /* private; wraps `zcash_client_backend::data_api::wallet::Proposal` */
}

impl Proposal {
    pub fn total_zat(&self) -> Zatoshis;
    pub fn fee_zat(&self) -> Zatoshis;
    pub fn expiry_height(&self) -> BlockHeight;  // ZIP-203
    pub fn output_count(&self) -> usize;
}

/// Result of a send.
#[derive(Clone, Debug)]
pub struct SendOutcome {
    pub tx_id: TxId,
    pub broadcast_at_height: BlockHeight,
    pub confirmation: Confirmation,
}

/// Future that resolves when the transaction reaches the configured confirmation depth.
pub struct Confirmation { /* private */ }

impl std::future::Future for Confirmation {
    type Output = Result<ConfirmedAt, WalletError>;
}

pub struct ConfirmedAt {
    pub tx_id: TxId,
    pub at_height: BlockHeight,
}

impl Wallet {
    pub async fn propose(
        &self,
        account_id: AccountId,
        recipient: PaymentRecipient,
        amount_zat: Zatoshis,
        memo: Option<Memo>,
        fee: FeeStrategy,
    ) -> Result<Proposal, WalletError>;

    pub async fn propose_payment_request(
        &self,
        account_id: AccountId,
        request: &PaymentRequest,
        fee: FeeStrategy,
    ) -> Result<Proposal, WalletError>;

    pub async fn send(
        &self,
        account_id: AccountId,
        idempotency: IdempotencyKey,
        proposal: Proposal,
        submitter: &dyn Submitter,
    ) -> Result<SendOutcome, WalletError>;

    pub async fn send_payment(
        &self,
        account_id: AccountId,
        idempotency: IdempotencyKey,
        recipient: PaymentRecipient,
        amount_zat: Zatoshis,
        memo: Option<Memo>,
        fee: FeeStrategy,
        submitter: &dyn Submitter,
    ) -> Result<SendOutcome, WalletError>;
}
```

### 1.3 `zally-storage` extensions

```rust
pub trait WalletStorage {
    // ... existing methods from Slices 1-2 ...

    /// Returns spendable balance for `account_id` as of the latest scanned height.
    async fn balance_for_account(
        &self,
        account_id: AccountId,
    ) -> Result<AccountBalance, StorageError>;

    /// Records `(idempotency_key, tx_id)` so a duplicate send returns the prior `tx_id`.
    /// not_retryable on `AccountNotFound`. retryable on transient I/O.
    async fn record_idempotent_send(
        &self,
        account_id: AccountId,
        idempotency: &IdempotencyKey,
        tx_id: TxId,
    ) -> Result<(), StorageError>;

    /// Looks up a prior send by idempotency key.
    async fn find_idempotent_send(
        &self,
        account_id: AccountId,
        idempotency: &IdempotencyKey,
    ) -> Result<Option<TxId>, StorageError>;
}

pub struct AccountBalance {
    pub total_zat: Zatoshis,
    pub spendable_zat: Zatoshis,
    pub pending_zat: Zatoshis,
}
```

### 1.4 Errors

`WalletError` gains variants for each ZIP guard:

```rust
pub enum WalletError {
    // ... existing variants ...
    #[error("memos are not permitted on transparent recipients (ZIP-302)")]
    MemoOnTransparentRecipient,
    #[error("TEX recipients (ZIP-320) require an all-transparent input set; this proposal includes shielded inputs")]
    ShieldedInputsOnTexRecipient,
    #[error("insufficient spendable balance: requested {requested_zat}, spendable {spendable_zat}")]
    InsufficientBalance { requested_zat: u64, spendable_zat: u64 },
    #[error("submitter error: {0}")]
    Submitter(#[from] zally_chain::SubmitterError),
    #[error("payment request parse failed: {reason}")]
    PaymentRequestParse { reason: String },
    #[error("proposal rejected by chain source or storage: {reason}")]
    ProposalRejected { reason: String },
}
```

### 1.5 Capability additions

`Capability::Zip302Memos`, `Capability::Zip320TexAddresses`, `Capability::Zip317ConventionalFee`, `Capability::IdempotentSend`.

---

## 2. Internal flow

### `Wallet::propose`

1. Validate input: `PaymentRecipient::network() == self.network()`, memo absent on Transparent or Tex, amount > 0.
2. Inside `spawn_blocking`: call `zcash_client_backend::data_api::wallet::propose_transfer` with the wallet's storage handle, the recipient, the ZIP-317 fee rule, and the latest scan height for anchor selection.
3. Wrap the returned `zcash_client_backend::Proposal` in Zally's `Proposal`.
4. Return.

### `Wallet::send`

1. Pre-flight: `storage.find_idempotent_send(account, idempotency)?`. If `Some(tx_id)`, return `SendOutcome` reconstructed from the prior send.
2. Inside `spawn_blocking`: call `create_proposed_transactions(... proposal, USK, OvkPolicy ...)` to produce signed transactions. The USK is derived from the sealed seed inline; zeroized before the `spawn_blocking` body returns.
3. For each produced transaction: `submitter.submit(raw_bytes).await`. On `Accepted` or `Duplicate`, record `(idempotency, tx_id)` in storage. On `Rejected`, return `WalletError::ProposalRejected`.
4. Build `Confirmation` future that resolves when `Wallet::observe()` sees `WalletEvent::TransactionConfirmed { tx_id, .. }` at depth `>= confirmation_depth_blocks_for(ReceiverPurpose)`.
5. Return `SendOutcome`.

`send_payment` is the one-shot wrapper: `propose` then `send`.

---

## 3. Tests

T0 unit:
- `PaymentRequest::from_uri` round trip for canonical ZIP-321 URIs.
- ZIP-302 guard: `Wallet::propose` with a memo and transparent recipient returns `MemoOnTransparentRecipient`.
- ZIP-320 guard: TEX recipient with shielded inputs returns `ShieldedInputsOnTexRecipient`.
- Idempotent reuse: two `send` calls with the same key return the same `tx_id`.

T1 integration (against `MockChainSource` + `MockSubmitter`):
- `propose_payment_request_round_trip.rs` — parse URI, propose, assert fee + output count.
- `send_idempotent_duplicate.rs` — first send accepted; second with same key returns the same `tx_id`.
- `send_emits_transaction_confirmed_event.rs` — `Wallet::observe()` receives `TransactionConfirmed` once the mock chain source signals it.

T3 live:
- `live_send_against_zinder_regtest.rs` — gated by `ZALLY_TEST_LIVE=1`; ignored until local node infrastructure lands.

---

## 4. Validation gate

Standard. Slice 3 introduces no new gate.

---

## 5. Open questions

### OQ-1: Real send vs. proposal-only delivery

Real `Wallet::send` requires balance, note selection, transaction construction (Sapling + Orchard provers), and chain integration that the mock cannot fully validate. Without live infrastructure, Slice 3's `send` returns `WalletError::InsufficientBalance` against `MockChainSource` (storage shows zero spendable). Two options:

- (a) Ship `propose` only in Slice 3; defer `send` to Slice 5 once live infrastructure is in place.
- (b) Ship both, with `send` exercising the API surface against an `InsufficientBalance` short-circuit.

Decision lean: (b). The send surface should be exercised by tests as soon as it lands; the failure mode is a real, named error variant, not a stub panic.

### OQ-2: `MockSubmitter` shape

The Slice 2 plan called for a `MockSubmitter` in `zally-testkit`. Slice 3 lands it: programmable accept/reject/duplicate outcomes per-call, with a test handle that lets the assertion read the bytes that were submitted.

### OQ-3: `Confirmation` future lifecycle

The future is driven by `Wallet::observe()`'s event stream. If the wallet handle is dropped before confirmation, the future returns `WalletError::WalletDropped`. Slice 3 ships this variant and the await-on-drop semantics; Slice 5 may add a `Confirmation::with_timeout(duration)` helper.

---

## 6. Acceptance

1. RFC-0003 accepted (three open questions resolved).
2. Implementation builds against this RFC; T0 + T1 tests pass under the validation gate.
3. Slice 3 PR cites RFC-0003.
