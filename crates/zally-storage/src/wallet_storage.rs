//! `WalletStorage` trait.

use async_trait::async_trait;
use zally_core::{AccountId, BlockHeight, IdempotencyKey, Network, TxId};
use zally_keys::SeedMaterial;
use zcash_client_backend::data_api::chain::ChainState;
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_keys::address::UnifiedAddress;

use crate::storage_error::StorageError;

/// Request body for [`WalletStorage::scan_blocks`].
///
/// Slice 5 wraps `zcash_client_backend::data_api::chain::scan_cached_blocks`. The storage
/// implementation owns access to the internal `WalletDb`; the caller supplies the compact
/// blocks (typically drained from a `ChainSource`), the height the first block is at, and
/// the `ChainState` valid for the block just before `from_height`.
pub struct ScanRequest {
    /// Compact blocks to scan, in ascending height order.
    pub blocks: Vec<CompactBlock>,
    /// Height of the first block in `blocks`. Must equal `from_state.block_height() + 1`.
    pub from_height: BlockHeight,
    /// Commitment tree state valid for the block at `from_height - 1`.
    pub from_state: ChainState,
}

impl ScanRequest {
    /// Constructs a scan request.
    #[must_use]
    pub fn new(
        blocks: Vec<CompactBlock>,
        from_height: BlockHeight,
        from_state: ChainState,
    ) -> Self {
        Self {
            blocks,
            from_height,
            from_state,
        }
    }
}

/// Outcome of a successful [`WalletStorage::scan_blocks`] call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ScanResult {
    /// Height the scanner advanced to (inclusive).
    pub scanned_to_height: BlockHeight,
    /// Number of blocks actually scanned.
    pub block_count: u64,
}

/// Request body for [`WalletStorage::propose_payment`].
///
/// Wraps `zcash_client_backend::data_api::wallet::propose_standard_transfer_to_address` so
/// the wallet layer can build a Zally proposal without depending on the upstream proposal
/// generics. The single-output ZIP-317 conventional fee path is the v1 contract; richer
/// multi-output and custom-fee plans land alongside their own request constructors.
pub struct ProposalPaymentRequest {
    /// Spending account.
    pub account_id: AccountId,
    /// Recipient address, already validated by the wallet layer (encoded string form).
    pub recipient_encoded: String,
    /// Amount to send, in zatoshis.
    pub amount_zat: u64,
    /// Optional memo (the wallet layer enforces the no-memo-on-transparent rule before this).
    pub memo: Option<Vec<u8>>,
}

impl ProposalPaymentRequest {
    /// Constructs a new request.
    #[must_use]
    pub fn new(
        account_id: AccountId,
        recipient_encoded: String,
        amount_zat: u64,
        memo: Option<Vec<u8>>,
    ) -> Self {
        Self {
            account_id,
            recipient_encoded,
            amount_zat,
            memo,
        }
    }
}

/// One prepared transaction returned by [`WalletStorage::prepare_payment`].
///
/// The transaction has been signed, persisted to the wallet DB, and is ready to broadcast.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PreparedTransaction {
    /// Transaction identifier.
    pub tx_id: TxId,
    /// Raw wire bytes ready for `Submitter::submit`.
    pub raw_bytes: Vec<u8>,
}

impl PreparedTransaction {
    /// Constructs a new prepared transaction record.
    #[must_use]
    pub fn new(tx_id: TxId, raw_bytes: Vec<u8>) -> Self {
        Self { tx_id, raw_bytes }
    }
}

/// One unspent shielded note row returned by [`WalletStorage::list_unspent_shielded_notes`].
///
/// Carries the upstream `zcash_protocol::ShieldedProtocol` directly so storage stays free of
/// chain-vocabulary deps. The wallet layer maps `protocol` onto its own `ShieldedPool`
/// vocabulary before returning to operators.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct UnspentShieldedNoteRow {
    /// Pool this note lives on.
    pub protocol: zcash_protocol::ShieldedProtocol,
    /// Value in zatoshis.
    pub value_zat: u64,
    /// Transaction that created the note.
    pub tx_id: TxId,
    /// Output index within the producing transaction (Sapling output index or Orchard
    /// action index, depending on `protocol`).
    pub output_index: u32,
    /// Height at which the note's producing transaction was mined.
    pub mined_height: BlockHeight,
}

/// One shielded note received by an account, in the form returned by
/// [`WalletStorage::received_shielded_notes_mined_in_range`].
///
/// Distinguishes itself from [`UnspentShieldedNoteRow`] by including notes that may have
/// been spent after they were received: the row reports the receive event, not the current
/// spendability state. Used by the wallet event stream to emit one
/// `ShieldedReceiveObserved` per note seen in the scanned block range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ReceivedShieldedNoteRow {
    /// Account that owns the note.
    pub account_id: AccountId,
    /// Pool this note lives on.
    pub protocol: zcash_protocol::ShieldedProtocol,
    /// Value in zatoshis.
    pub value_zat: u64,
    /// Transaction that created the note.
    pub tx_id: TxId,
    /// Output index within the producing transaction.
    pub output_index: u32,
    /// Height at which the note's producing transaction was mined.
    pub mined_height: BlockHeight,
}

/// Summary of a successful [`WalletStorage::propose_payment`] call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ProposalSummary {
    /// Sum of payment outputs.
    pub total_zat: u64,
    /// Fee paid by the proposal.
    pub fee_zat: u64,
    /// Min chain height required to execute the proposal.
    pub min_target_height: BlockHeight,
    /// Number of payment outputs in the proposal.
    pub output_count: usize,
}

/// Trait abstracting the wallet database.
///
/// Slice 1 exposes the subset needed to open, create, and derive the first address. Later
/// slices extend with sync, balance, spend, and event APIs. Every implementation returns
/// [`StorageError`]; backends with a native error type translate inside their impl.
#[async_trait]
pub trait WalletStorage: Send + Sync + 'static {
    /// Opens or creates the wallet database. Runs schema migrations before returning.
    /// Idempotent.
    ///
    /// `retryable` on transient I/O. `requires_operator` on schema migration failure.
    async fn open_or_create(&self) -> Result<(), StorageError>;

    /// Creates the wallet's first account for `seed`, anchored at `prior_chain_state`.
    ///
    /// `prior_chain_state` is the note commitment tree state at the block immediately before
    /// the operator-chosen birthday height. It carries the real Sapling and Orchard frontiers
    /// fetched from the chain source; passing empty frontiers at a non-genesis birthday
    /// makes the first call to [`WalletStorage::scan_blocks`] fail closed with
    /// `NonSequentialBlocks` because the wallet's commitment tree progression no longer
    /// matches the chain.
    ///
    /// The Slice 1 invariant is one account per wallet: a second call returns
    /// [`StorageError::AccountAlreadyExists`].
    ///
    /// `not_retryable` on `AccountAlreadyExists`. `retryable` on transient I/O.
    /// `requires_operator` on migration mismatch.
    async fn create_account_for_seed(
        &self,
        seed: &SeedMaterial,
        prior_chain_state: ChainState,
    ) -> Result<AccountId, StorageError>;

    /// Looks up the [`AccountId`] for the account whose UFVK matches the seed.
    ///
    /// Returns `Ok(None)` if no account matches.
    ///
    /// `not_retryable`.
    async fn find_account_for_seed(
        &self,
        seed: &SeedMaterial,
    ) -> Result<Option<AccountId>, StorageError>;

    /// Generates, persists, and marks as exposed the next-available Unified Address for
    /// `account_id` with Sapling + Orchard receivers (no transparent). Repeated calls walk
    /// forward through diversifier indices per ZIP-316. Free of the transparent gap-limit;
    /// suitable as the default receive-address allocator.
    ///
    /// `not_retryable` on unknown account. `retryable` on transient I/O.
    async fn derive_next_address(
        &self,
        account_id: AccountId,
    ) -> Result<UnifiedAddress, StorageError>;

    /// Generates a Unified Address that also carries a transparent (P2PKH) receiver.
    ///
    /// Subject to the BIP-44 transparent gap limit: the upstream pre-generates 10 addresses
    /// ahead of the reservation, so on a fresh wallet only one call succeeds before an
    /// on-chain transaction must credit one of the reserved transparent addresses. Use
    /// [`Self::derive_next_address`] for the unbounded shielded-only stream.
    ///
    /// `not_retryable` on gap-limit exhaustion or unknown account; `retryable` on transient I/O.
    async fn derive_next_address_with_transparent(
        &self,
        account_id: AccountId,
    ) -> Result<UnifiedAddress, StorageError>;

    /// Returns the network this storage instance was opened for.
    fn network(&self) -> Network;

    /// Scans `request.blocks` into the wallet, persisting decrypted notes and updating the
    /// chain tip.
    ///
    /// Slice 5 implementation drives `zcash_client_backend::data_api::chain::scan_cached_blocks`
    /// against the wallet's internal database. The caller drains compact blocks from a
    /// `ChainSource` and supplies the corresponding `ChainState` for the block immediately
    /// below `request.from_height`.
    ///
    /// `not_retryable` on malformed blocks or chain-state mismatch; `retryable` on transient I/O.
    async fn scan_blocks(&self, request: ScanRequest) -> Result<ScanResult, StorageError>;

    /// Returns the height the wallet has been fully scanned to, or `None` for a fresh wallet.
    async fn fully_scanned_height(&self) -> Result<Option<BlockHeight>, StorageError>;

    /// Returns the wallet's birthday height, the earliest block the operator-configured
    /// account expects to receive funds at. `Wallet::sync` starts from this height on a
    /// fresh wallet (rather than genesis) to keep cold sync proportional to the operator's
    /// real receive window.
    ///
    /// Returns `Ok(None)` for a wallet with no initialised account.
    async fn wallet_birthday(&self) -> Result<Option<BlockHeight>, StorageError>;

    /// Builds a ZIP-317 conventional-fee proposal for `request`, creates the signed
    /// transactions via `zcash_client_backend::data_api::wallet::create_proposed_transactions`,
    /// and returns the raw transaction bytes (one per step) for the caller to submit via
    /// a `Submitter` (see `zally_chain::Submitter`).
    ///
    /// The storage layer constructs a `LocalTxProver` using the default params location
    /// (`~/.local/share/ZcashParams/` on macOS, `~/.zcash-params/` on Linux). Returns
    /// `StorageError::ProverUnavailable` if the params are not present.
    ///
    /// `not_retryable` on insufficient balance or invalid recipient; `retryable` on
    /// transient I/O.
    async fn prepare_payment(
        &self,
        request: ProposalPaymentRequest,
        seed: &SeedMaterial,
    ) -> Result<Vec<PreparedTransaction>, StorageError>;

    /// Builds a ZIP-317 conventional-fee proposal for `request` against the wallet's
    /// available shielded notes and transparent UTXOs.
    ///
    /// Calls `zcash_client_backend::data_api::wallet::propose_standard_transfer_to_address`
    /// against the wallet DB. The returned `ProposalSummary` is the Zally-facing view of the
    /// upstream `Proposal`; storage retains no proposal state between calls.
    ///
    /// `not_retryable` on insufficient balance or invalid recipient; `retryable` on transient
    /// I/O.
    async fn propose_payment(
        &self,
        request: ProposalPaymentRequest,
    ) -> Result<ProposalSummary, StorageError>;

    /// Builds an unsigned PCZT (raw bytes) for `request` by composing
    /// `propose_standard_transfer_to_address` and
    /// `zcash_client_backend::data_api::wallet::create_pczt_from_proposal`.
    ///
    /// The returned bytes are not yet authorized; the wallet layer wraps them as
    /// `zally_pczt::PcztBytes` and routes through the `Signer` role before extraction.
    ///
    /// `not_retryable` on insufficient balance or invalid recipient; `retryable` on transient
    /// I/O.
    async fn create_pczt(&self, request: ProposalPaymentRequest) -> Result<Vec<u8>, StorageError>;

    /// Extracts a finalised PCZT, persists the resulting transaction in the wallet DB, and
    /// returns its raw bytes plus `tx_id`.
    ///
    /// Wraps `zcash_client_backend::data_api::wallet::extract_and_store_transaction_from_pczt`.
    /// Loads Sapling verifying keys from the platform-default `ZcashParams` location.
    ///
    /// `not_retryable` on malformed PCZTs or missing authorizations; `requires_operator` on
    /// missing parameters; `retryable` on transient I/O.
    async fn extract_and_store_pczt(
        &self,
        pczt_bytes: Vec<u8>,
    ) -> Result<PreparedTransaction, StorageError>;

    /// Returns the [`TxId`] previously recorded for `key`, or `None` when the key has not
    /// been used. Backed by a Zally-owned `ext_zally_idempotency` table colocated with the
    /// wallet database.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn lookup_idempotent_submission(
        &self,
        key: &IdempotencyKey,
    ) -> Result<Option<TxId>, StorageError>;

    /// Records `(key, tx_id)` so subsequent [`WalletStorage::lookup_idempotent_submission`]
    /// calls with the same `key` return the same `tx_id`.
    ///
    /// Returns [`StorageError::IdempotencyKeyConflict`] when `key` is already bound to a
    /// different `tx_id`; the wallet layer should surface the prior `tx_id` to the caller
    /// rather than overwrite the ledger.
    ///
    /// `not_retryable` on conflict; `retryable` on transient I/O.
    async fn record_idempotent_submission(
        &self,
        key: IdempotencyKey,
        tx_id: TxId,
    ) -> Result<(), StorageError>;

    /// Returns every wallet-owned transaction whose mined height lies in the inclusive range
    /// `[start, end]`, paired with that height. The order is ascending by mined height.
    ///
    /// Reads `zcash_client_sqlite`'s `transactions.mined_height` column directly through a
    /// side `SQLite` connection; the column is the canonical "confirmed at this height"
    /// indicator. Used by [`crate::WalletStorage`] consumers to drive `TransactionConfirmed`
    /// events after each scan advances the wallet.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn wallet_tx_ids_mined_in_range(
        &self,
        start: BlockHeight,
        end: BlockHeight,
    ) -> Result<Vec<(TxId, BlockHeight)>, StorageError>;

    /// Returns the highest chain tip the wallet has observed across all prior syncs, or
    /// `None` for a fresh wallet that has never recorded a tip.
    ///
    /// Decoupled from `fully_scanned_height`: a sync that fetched zero compact blocks still
    /// records an observed tip if the chain source reported one. Used to detect reorgs by
    /// comparing the current chain tip against the recorded observed tip.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn lookup_observed_tip(&self) -> Result<Option<BlockHeight>, StorageError>;

    /// Records `new_tip` as the latest observed chain tip. Idempotent: if the recorded tip
    /// is already greater than or equal to `new_tip`, the call is a no-op and the prior
    /// value is preserved. Callers detect reorgs by reading [`Self::lookup_observed_tip`]
    /// and comparing against the chain source's current tip before this call.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn record_observed_tip(&self, new_tip: BlockHeight) -> Result<(), StorageError>;

    /// Returns every unspent Sapling and Orchard note owned by `account_id` against
    /// `target_height` (typically the wallet's current chain tip). Spent notes, locked
    /// notes, and notes whose producing transaction has not yet mined are excluded.
    ///
    /// Wraps `zcash_client_backend::data_api::InputSource::select_unspent_notes`. Confirmation
    /// count is not computed here; callers derive it from `mined_height` against their own
    /// tip-of-interest.
    ///
    /// `not_retryable` on unknown account or schema errors; `retryable` on transient I/O.
    async fn list_unspent_shielded_notes(
        &self,
        account_id: AccountId,
        target_height: BlockHeight,
    ) -> Result<Vec<UnspentShieldedNoteRow>, StorageError>;

    /// Returns every Sapling and Orchard note received in the inclusive height range
    /// `[from_height, to_height]`, regardless of current spent state.
    ///
    /// Powers the wallet's event stream: after a successful `scan_blocks` call the wallet
    /// looks up notes newly mined in the scanned range and emits one
    /// `ShieldedReceiveObserved` per row. Spent state is intentionally not filtered here
    /// so consumers see every receive that ever happened in the range, suitable for
    /// donation indexing and historical aggregation.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn received_shielded_notes_mined_in_range(
        &self,
        from_height: BlockHeight,
        to_height: BlockHeight,
    ) -> Result<Vec<ReceivedShieldedNoteRow>, StorageError>;
}
