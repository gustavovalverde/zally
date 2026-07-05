//! `WalletStorage` trait.

use async_trait::async_trait;
use zally_core::{
    AccountId, BlockHeight, HoldId, IdempotencyKey, Network, OutPoint, TxId, Zatoshis,
};

/// The storage backend behind a wallet handle.
///
/// Returned by [`WalletStorage::kind`] so the wallet capability descriptor can advertise the
/// in-use backend without `std::any::type_name::<St>()` introspection.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum StorageKind {
    /// `zally_storage::Sqlite`.
    Sqlite,
    /// A custom storage backend provided by the operator.
    Custom,
}
use zally_keys::SeedMaterial;
use zcash_client_backend::data_api::chain::ChainState;
use zcash_client_backend::data_api::scanning::ScanRange;
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_keys::address::UnifiedAddress;
use zcash_protocol::ShieldedPool;

use crate::account_balance_row::AccountBalanceRow;
use crate::error::StorageError;
use crate::exposed_address_row::ExposedAddressRow;
use crate::hold_row::{HeldNote, HoldRow};
use crate::pending_broadcast_input_row::PendingBroadcastInputRow;

/// Request body for [`WalletStorage::scan_blocks`].
///
/// Wraps `zcash_client_backend::data_api::chain::scan_cached_blocks`. The storage
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

/// Wallet note-commitment tree roots at a checkpoint height.
///
/// Each field is `None` when the wallet holds no checkpoint at the requested height (the
/// scanner prunes old checkpoints). Compare these against the chain's tree-state root at the
/// same height to detect commitment-tree corruption, which surfaces downstream as shielded
/// proofs the network rejects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CommitmentTreeRoots {
    /// Sapling note-commitment tree root, little-endian, or `None` if no checkpoint matches.
    pub sapling: Option<[u8; 32]>,
    /// Orchard note-commitment tree root, little-endian, or `None` if no checkpoint matches.
    pub orchard: Option<[u8; 32]>,
}

/// Request body for [`WalletStorage::propose_payment`].
///
/// Wraps `zcash_client_backend::data_api::wallet::propose_standard_transfer_to_address` so
/// the wallet layer can build a Zally proposal without depending on the upstream proposal
/// generics. Carries the single-output ZIP-317 conventional fee path; richer multi-output
/// and custom-fee plans land alongside their own request constructors.
///
/// The storage backend transparently excludes wallet-owned outpoints that are locked by a
/// still-unconfirmed broadcast at its `InputSource::get_spendable_transparent_outputs`
/// override; callers do not pass an exclusion set.
pub struct ProposalPaymentRequest {
    /// Spending account.
    pub account_id: AccountId,
    /// Recipient address, already validated by the wallet layer (encoded string form).
    pub recipient_encoded: String,
    /// Amount to send.
    pub amount_zat: Zatoshis,
    /// Optional memo (the wallet layer enforces the no-memo-on-transparent rule before this).
    pub memo: Option<Vec<u8>>,
}

impl ProposalPaymentRequest {
    /// Constructs a payment request.
    #[must_use]
    pub const fn new(
        account_id: AccountId,
        recipient_encoded: String,
        amount_zat: Zatoshis,
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

/// Request body for [`WalletStorage::shield_transparent_funds`].
///
/// Shields wallet-owned transparent UTXOs into the account's internal shielded receiver.
/// The storage backend transparently excludes outpoints locked by a still-unconfirmed
/// wallet-owned broadcast at the `InputSource::get_spendable_transparent_outputs` override.
pub struct ShieldTransparentRequest {
    /// Account that owns the transparent UTXOs and receives the shielded output.
    pub account_id: AccountId,
    /// Minimum total transparent input value to shield.
    pub shielding_threshold_zat: Zatoshis,
}

impl ShieldTransparentRequest {
    /// Constructs a transparent shielding request.
    #[must_use]
    pub const fn new(account_id: AccountId, shielding_threshold_zat: Zatoshis) -> Self {
        Self {
            account_id,
            shielding_threshold_zat,
        }
    }
}

/// One prepared transaction returned by [`WalletStorage::prepare_payment`] or
/// [`WalletStorage::shield_transparent_funds`].
///
/// The transaction has been signed, persisted to the wallet DB, and is ready to broadcast.
/// `transparent_inputs` carries the outpoints the proposal selected so callers can record a
/// pending-broadcast row without re-parsing the raw transaction.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PreparedTransaction {
    /// Transaction identifier.
    pub tx_id: TxId,
    /// Raw wire bytes ready for `Submitter::submit`.
    pub raw_bytes: Vec<u8>,
    /// Transparent outpoints consumed by this transaction, with their values.
    pub transparent_inputs: Vec<(OutPoint, Zatoshis)>,
    /// Block height at which Zcash consensus will reject this transaction if it has
    /// not been mined. Callers use this to bound the time they wait for confirmation
    /// before classifying the broadcast as expired.
    pub tx_expiry_height: BlockHeight,
}

impl PreparedTransaction {
    /// Constructs a prepared transaction record.
    #[must_use]
    pub const fn new(
        tx_id: TxId,
        raw_bytes: Vec<u8>,
        transparent_inputs: Vec<(OutPoint, Zatoshis)>,
        tx_expiry_height: BlockHeight,
    ) -> Self {
        Self {
            tx_id,
            raw_bytes,
            transparent_inputs,
            tx_expiry_height,
        }
    }
}

/// One unspent shielded note row returned by [`WalletStorage::list_unspent_shielded_notes`].
///
/// Carries the upstream `zcash_protocol::ShieldedPool` directly so storage stays free of
/// chain-vocabulary deps. The wallet layer maps `protocol` onto its own `ShieldedPool`
/// vocabulary before returning to operators.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct UnspentShieldedNoteRow {
    /// Pool this note lives on.
    pub protocol: zcash_protocol::ShieldedPool,
    /// Spendable value in this note.
    pub value_zat: Zatoshis,
    /// Transaction that created the note.
    pub tx_id: TxId,
    /// Output index within the producing transaction (Sapling output index or Orchard
    /// action index, depending on `protocol`).
    pub output_index: u32,
    /// Height at which the note's producing transaction was mined.
    pub mined_height: BlockHeight,
}

/// One shielded note received by an account, in the form returned by
/// [`WalletStorage::received_shielded_notes_mined_in_range`] and
/// [`WalletStorage::list_shielded_receives_for_account`].
///
/// Distinguishes itself from [`UnspentShieldedNoteRow`] by including notes that may have
/// been spent after they were received: the row reports the receive event, not the current
/// spendability state. Used by the wallet event stream to emit one
/// `ShieldedReceiveObserved` per note seen in the scanned block range, and by the inflow
/// classifier to attribute receives to one of three provenance categories.
///
/// `is_change` and `spent_our_inputs` together let a consumer label whether the wallet
/// itself produced this receive. A self-funded change output sets both true; a transparent
/// or shielded sweep of the wallet's own funds sets `spent_our_inputs` true without
/// `is_change`; a third-party transfer leaves both false.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ReceivedShieldedNoteRow {
    /// Account that owns the note.
    pub account_id: AccountId,
    /// Pool this note lives on.
    pub protocol: zcash_protocol::ShieldedPool,
    /// Note value at the time of receipt.
    pub value_zat: Zatoshis,
    /// Transaction that created the note.
    pub tx_id: TxId,
    /// Output index within the producing transaction.
    pub output_index: u32,
    /// Height at which the note's producing transaction was mined.
    pub mined_height: BlockHeight,
    /// Block header timestamp in milliseconds (Unix epoch), sourced from the
    /// `blocks.time` column at the row's `mined_height`. The authoritative receive
    /// time across both the live event path and historical replay.
    pub block_timestamp_ms: u64,
    /// True when `zcash_client_sqlite` marked this note as change for the receiving
    /// account, meaning the receiving account also spent notes in the producing
    /// transaction and the note is internally-scoped.
    pub is_change: bool,
    /// True when the producing transaction's input set includes any UTXO or shielded
    /// note owned by the receiving account, across the Sapling, Orchard, and
    /// transparent pools. False for third-party transfers.
    pub spent_our_inputs: bool,
}

/// One transparent receiver the wallet should refresh through a chain source.
///
/// Compact blocks do not carry enough transparent-output detail for wallet discovery.
/// The wallet sync loop asks storage for the exposed transparent receivers and then asks
/// `ChainSource` for matching UTXOs.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TransparentReceiverRow {
    /// Account that owns this receiver.
    pub account_id: AccountId,
    /// Receiver `scriptPubKey` bytes.
    pub script_pub_key_bytes: Vec<u8>,
}

impl TransparentReceiverRow {
    /// Constructs a transparent receiver row.
    #[must_use]
    pub fn new(account_id: AccountId, script_pub_key_bytes: Vec<u8>) -> Self {
        Self {
            account_id,
            script_pub_key_bytes,
        }
    }
}

/// One transparent UTXO discovered by a chain source and ready to record in storage.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TransparentUtxoRow {
    /// Transaction that produced this output.
    pub tx_id: TxId,
    /// Output index within the producing transaction.
    pub output_index: u32,
    /// UTXO value.
    pub value_zat: Zatoshis,
    /// Height at which this output was mined.
    pub mined_height: BlockHeight,
    /// Output `scriptPubKey` bytes.
    pub script_pub_key_bytes: Vec<u8>,
}

impl TransparentUtxoRow {
    /// Constructs a transparent UTXO row.
    #[must_use]
    pub fn new(
        tx_id: TxId,
        output_index: u32,
        value_zat: Zatoshis,
        mined_height: BlockHeight,
        script_pub_key_bytes: Vec<u8>,
    ) -> Self {
        Self {
            tx_id,
            output_index,
            value_zat,
            mined_height,
            script_pub_key_bytes,
        }
    }
}

/// Summary of a successful [`WalletStorage::propose_payment`] call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ProposalSummary {
    /// Sum of payment outputs.
    pub total_zat: Zatoshis,
    /// Fee paid by the proposal.
    pub fee_zat: Zatoshis,
    /// Min chain height required to execute the proposal.
    pub min_target_height: BlockHeight,
    /// Number of payment outputs in the proposal.
    pub output_count: usize,
}

/// Request body for [`WalletStorage::create_hold`].
///
/// Carries everything the storage layer needs to persist a fresh dispense reservation
/// row and enforce the "amount sum stays within spendable" invariant atomically with
/// any concurrent reservation attempt.
#[derive(Clone, Debug)]
pub struct HoldRecord {
    /// Wallet-issued identifier for the new reservation.
    pub hold_id: HoldId,
    /// Caller-supplied request identifier (idempotency anchor) for the reservation.
    pub request_id: IdempotencyKey,
    /// Caller-supplied idempotency key for the eventual broadcast.
    pub idempotency_key: IdempotencyKey,
    /// Account the reservation belongs to.
    pub account_id: AccountId,
    /// Amount the caller wants to reserve.
    pub amount_zat: Zatoshis,
    /// Spendable amount the wallet observed at reservation time, used by the storage
    /// layer to enforce the amount-sum invariant. Callers compute this as
    /// `total_spendable - sum_active_reservations`; the storage layer rechecks the same
    /// invariant inside its sqlite transaction so two concurrent reservations cannot
    /// both pass an overlapping pre-check.
    pub spendable_for_check_zat: Zatoshis,
    /// Notes the wallet considered locked at reservation time. Informational only;
    /// the enforcement contract is amount-based.
    pub locked_notes: Vec<HeldNote>,
    /// Unix milliseconds when the reservation was recorded.
    pub reserved_at_ms: u64,
}

/// Request body for [`WalletStorage::record_pending_broadcast_inputs`].
///
/// Carries the metadata a wallet-owned broadcast needs to register itself with the
/// pending-broadcast filter: the broadcast txid, the account it belongs to, when the
/// broadcast happened (wall-clock and chain tip), and the transparent outpoints the
/// transaction consumes (each paired with its value).
#[derive(Clone, Debug)]
pub struct PendingBroadcastRecord {
    /// Identifier of the wallet-owned transaction that consumed the outpoints.
    pub broadcast_tx_id: TxId,
    /// Account that owns the broadcast.
    pub account_id: AccountId,
    /// Unix milliseconds when the wallet recorded the broadcast.
    pub broadcast_at_ms: u64,
    /// Chain tip the wallet had observed at broadcast time. `None` when no tip was recorded.
    pub broadcast_at_height: Option<BlockHeight>,
    /// Each transparent input the broadcast consumes, paired with its value.
    pub inputs: Vec<(OutPoint, zally_core::Zatoshis)>,
}

/// Trait abstracting the wallet database.
///
/// Every implementation returns [`StorageError`]; backends with a native error type translate
/// inside their impl.
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
    /// Zally holds one account per wallet: a second call returns
    /// [`StorageError::AccountAlreadyExists`].
    ///
    /// `not_retryable` on `AccountAlreadyExists`. `retryable` on transient I/O.
    /// `requires_operator` on migration mismatch.
    async fn create_account_for_seed(
        &self,
        seed: &SeedMaterial,
        prior_chain_state: ChainState,
    ) -> Result<AccountId, StorageError>;

    /// Deletes the wallet database and recreates it with a fresh account for `seed`,
    /// anchored at `prior_chain_state`, in one atomic storage operation.
    ///
    /// The deepest rung of the sync driver's repair ladder: wallet chain state is
    /// disposable derived state, so when it can no longer be reconciled with the chain
    /// (for example a note commitment tree conflict that a bounded rewind cannot clear)
    /// the repair is to rebuild from the birthday.
    ///
    /// This discards ALL wallet-local state, including the zally ledger: send idempotency
    /// records, holds, and pending broadcasts are gone. Derived chain state is rebuilt by
    /// rescan; the ledger is not. Hosts that need cross-rebuild idempotency must keep
    /// their own ledger.
    ///
    /// The returned [`AccountId`] equals the one the discarded database held for the same
    /// seed: account identity is key identity, not database identity.
    ///
    /// `requires_operator` on migration failure. `retryable` on transient I/O.
    async fn recreate_with_account(
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

    /// Returns every wallet-owned transparent receiver that should be refreshed from the
    /// chain source.
    ///
    /// Wraps `WalletRead::get_transparent_receivers` for every wallet account. The caller
    /// queries the chain source by `script_pub_key_bytes`, then records the resulting UTXOs
    /// through [`Self::record_transparent_utxos`].
    ///
    /// `not_retryable` on unknown account or schema errors; `retryable` on transient I/O.
    async fn list_transparent_receivers(&self)
    -> Result<Vec<TransparentReceiverRow>, StorageError>;

    /// Records transparent UTXOs discovered from a chain source.
    ///
    /// Wraps `WalletWrite::put_received_transparent_utxo`. The operation is idempotent at
    /// the upstream wallet database boundary: recording a UTXO that already exists returns
    /// success without duplicating spendable funds.
    ///
    /// `not_retryable` on malformed output scripts or schema errors; `retryable` on
    /// transient I/O.
    async fn record_transparent_utxos(
        &self,
        utxos: Vec<TransparentUtxoRow>,
    ) -> Result<u64, StorageError>;

    /// Returns the network this storage instance was opened for.
    fn network(&self) -> Network;

    /// Returns the storage backend kind for the wallet capability descriptor. Default is
    /// [`StorageKind::Custom`]; first-party implementations override.
    fn kind(&self) -> StorageKind {
        StorageKind::Custom
    }

    /// Scans `request.blocks` into the wallet, persisting decrypted notes and updating the
    /// chain tip.
    ///
    /// Drives `zcash_client_backend::data_api::chain::scan_cached_blocks` against the
    /// wallet's internal database. The caller drains compact blocks from a `ChainSource`
    /// and supplies the corresponding `ChainState` for the block immediately below
    /// `request.from_height`.
    ///
    /// `not_retryable` on malformed blocks or chain-state mismatch; `retryable` on transient I/O.
    async fn scan_blocks(&self, request: ScanRequest) -> Result<ScanResult, StorageError>;

    /// Returns the scan ranges the wallet wants scanned, highest priority first.
    ///
    /// Wraps `WalletRead::suggest_scan_ranges`. A `Verify`-priority range, when present, is
    /// always first; it must be scanned before any other range to catch reorgs. The ranges
    /// are aligned to commitment-tree subtree boundaries, so the chain state for
    /// `range.start - 1` is always available at a real frontier.
    async fn suggest_scan_ranges(&self) -> Result<Vec<ScanRange>, StorageError>;

    /// Records commitment-tree subtree roots for `pool`, starting at subtree index
    /// `start_index`. Each entry is `(subtree_end_height, 32-byte root hash)`.
    ///
    /// Wraps `WalletCommitmentTrees::put_{sapling,orchard}_subtree_roots`. Priming these is
    /// what lets the wallet witness notes in a subtree without scanning every block it spans,
    /// and is a prerequisite for `suggest_scan_ranges` to plan subtree-aligned work.
    ///
    /// `pool = ShieldedPool::Ironwood` returns `StorageError::ShieldedPoolUnsupported`; the
    /// pinned `zcash_client_backend` has no Ironwood subtree-root write path yet.
    async fn put_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u64,
        roots: Vec<(BlockHeight, [u8; 32])>,
    ) -> Result<(), StorageError>;

    /// Returns the height the wallet has been fully scanned to, or `None` for a fresh wallet.
    async fn fully_scanned_height(&self) -> Result<Option<BlockHeight>, StorageError>;

    /// Computes the wallet's current Sapling and Orchard note-commitment tree roots over every
    /// leaf the wallet has appended.
    ///
    /// Wraps `WalletCommitmentTrees::with_{sapling,orchard}_tree_mut` plus
    /// `ShardTree::root_at_checkpoint_depth(None)`, which needs no checkpoint (the scanner does
    /// not retain a checkpoint at every height). After the wallet scans up to height `H`, these
    /// roots equal the chain's tree-state root at `H` if and only if the wallet assembled the
    /// tree correctly; a mismatch means a corrupt tree and a bad spend anchor.
    async fn commitment_tree_roots(&self) -> Result<CommitmentTreeRoots, StorageError>;

    /// Returns the account's birthday height, the earliest block the operator-configured
    /// account expects to receive funds at. `Wallet::sync` starts from this height on a
    /// fresh wallet (rather than genesis) to keep cold sync proportional to the operator's
    /// real receive window, and a full rebuild of derived state rescans from it.
    ///
    /// Wraps `WalletRead::get_wallet_birthday`.
    ///
    /// `not_retryable` on missing account; `retryable` on transient I/O.
    async fn account_birthday(&self) -> Result<BlockHeight, StorageError>;

    /// Builds a ZIP-317 conventional-fee proposal for `request`, creates the signed
    /// transactions via `zcash_client_backend::data_api::wallet::create_proposed_transactions`,
    /// and returns the raw transaction bytes (one per step) for the caller to submit via
    /// a `Submitter` (see `zally_chain::Submitter`).
    ///
    /// `excluded_outpoints` is the set of transparent outpoints locked by a still-unconfirmed
    /// wallet-owned broadcast; the storage layer filters these out at the
    /// `InputSource::get_spendable_transparent_outputs` seam so the proposal selector never
    /// picks them. An empty set disables the filter.
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
        excluded_outpoints: std::collections::HashSet<OutPoint>,
        seed: &SeedMaterial,
    ) -> Result<Vec<PreparedTransaction>, StorageError>;

    /// Shields wallet-owned transparent UTXOs into the account's internal shielded receiver.
    ///
    /// `excluded_outpoints` follows the same contract as on `prepare_payment`. Empty set
    /// disables the filter.
    ///
    /// Wraps `zcash_client_backend::data_api::wallet::shield_transparent_funds` using ZIP-317
    /// conventional fees and the default ZIP-315 confirmations policy. The storage layer
    /// selects from the account's known transparent receivers; the wallet layer is responsible
    /// for refreshing transparent UTXOs from a chain source before calling this method.
    ///
    /// `not_retryable` on insufficient transparent funds, unknown account, or missing
    /// transparent receivers; `requires_operator` on missing Sapling params; `retryable` on
    /// transient I/O.
    async fn shield_transparent_funds(
        &self,
        request: ShieldTransparentRequest,
        excluded_outpoints: std::collections::HashSet<OutPoint>,
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
    /// When `target_expiry_height` is set, the caller-supplied value lands on
    /// `Global::expiry_height` before the upstream IO Finalizer step. The IO
    /// Finalizer signs every dummy orchard action with its `dummy_sk` against
    /// the shielded sighash computed from that global and then consumes
    /// `dummy_sk`. Setting expiry post-finalization (via a wire-format Updater)
    /// would invalidate those dummy signatures with no recovery path, so this
    /// is the only stage at which the caller can pin a value.
    ///
    /// The returned bytes are not yet authorized; the wallet layer wraps them as
    /// `zally_pczt::PcztBytes` and routes through the `Signer` role before extraction.
    ///
    /// `not_retryable` on insufficient balance or invalid recipient; `retryable` on transient
    /// I/O.
    async fn create_pczt(
        &self,
        request: ProposalPaymentRequest,
        target_expiry_height: Option<BlockHeight>,
    ) -> Result<Vec<u8>, StorageError>;

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
    async fn find_idempotent_submission(
        &self,
        key: &IdempotencyKey,
    ) -> Result<Option<TxId>, StorageError>;

    /// Records `(key, tx_id)` so subsequent [`WalletStorage::find_idempotent_submission`]
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

    /// Returns the chain tip recorded by the most recent sync, or `None` for a fresh wallet
    /// that has never recorded a tip.
    ///
    /// Decoupled from `fully_scanned_height`: a sync that fetched zero compact blocks still
    /// records an observed tip if the chain source reported one. Used to detect reorgs by
    /// comparing the current chain tip against the recorded observed tip.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn find_observed_tip(&self) -> Result<Option<BlockHeight>, StorageError>;

    /// Records `new_tip` as the most recently observed chain tip, unconditionally: the
    /// stored value always reflects the last observation, even when `new_tip` is lower than
    /// a previously recorded tip. Reorg detection depends on this: a monotonic high-water
    /// mark would hide a tip regress. Callers detect reorgs by reading
    /// [`Self::find_observed_tip`] and comparing against the chain source's current tip
    /// before this call.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn record_observed_tip(&self, new_tip: BlockHeight) -> Result<(), StorageError>;

    /// Informs the underlying `WalletDb` of the raw chain tip so transaction proposals
    /// compute a valid `expiry_height` against the live chain.
    ///
    /// This sets the chain tip used for proposal and expiry math only; it does not scan
    /// blocks. The wallet still scans only up to the chain source's finalized height (see
    /// [`Self::scan_blocks`]). Without this call the `WalletDb` would believe the tip is the
    /// last scanned (finalized) height and build transactions whose expiry has already
    /// passed by the time they reach the network.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn update_chain_tip(&self, tip_height: BlockHeight) -> Result<(), StorageError>;

    /// Returns every unspent Sapling, Orchard, and Ironwood note owned by `account_id` against
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

    /// Returns every Unified Address previously exposed for `account_id`, in derivation
    /// order (ascending by exposure height, then by diversifier index).
    ///
    /// Read-only counterpart to [`Self::derive_next_address`] and
    /// [`Self::derive_next_address_with_transparent`]. Calling this method never advances
    /// a diversifier index and never burns a transparent gap-limit slot.
    ///
    /// `not_retryable` on unknown account or schema errors; `retryable` on transient I/O.
    async fn list_exposed_addresses(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<ExposedAddressRow>, StorageError>;

    /// Records the transparent inputs of a wallet-owned transaction that was just broadcast.
    ///
    /// Used by the spend path immediately after `zally_chain::Submitter::submit` returns
    /// success. The recorded rows are what
    /// [`Self::list_pending_broadcast_inputs`] returns and what the input-selection filter
    /// excludes from new spends, so callers must record every transparent input the
    /// broadcast consumed.
    ///
    /// Inserts are idempotent on `(broadcast_tx_id, outpoint_tx_id, outpoint_index)`: a
    /// second call with the same triple replaces the prior `broadcast_at_*` fields.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn record_pending_broadcast_inputs(
        &self,
        record: PendingBroadcastRecord,
    ) -> Result<(), StorageError>;

    /// Returns every pending broadcast input owned by `account_id` whose `broadcast_at_ms`
    /// is at or after `after_at_ms`. Older rows are filtered out so the caller does not
    /// need to apply the inflight-window cutoff itself.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn list_pending_broadcast_inputs(
        &self,
        account_id: AccountId,
        after_at_ms: u64,
    ) -> Result<Vec<PendingBroadcastInputRow>, StorageError>;

    /// Drops every pending-broadcast row whose `broadcast_tx_id` appears in `tx_ids`.
    ///
    /// Called by `Wallet::sync` after each scan to retire pending entries whose spending
    /// transaction is now observed mined.
    ///
    /// Returns the number of rows removed.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn clear_pending_broadcast_inputs_for_mined(
        &self,
        tx_ids: &[TxId],
    ) -> Result<u64, StorageError>;

    /// Drops every pending-broadcast row whose `broadcast_at_ms` is strictly before
    /// `before_at_ms`.
    ///
    /// Called by `Wallet::sync` once per cycle so a permanently-dropped broadcast
    /// eventually frees its locked outpoints.
    ///
    /// Returns the number of rows removed.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn clear_expired_pending_broadcast_inputs(
        &self,
        before_at_ms: u64,
    ) -> Result<u64, StorageError>;

    /// Returns the per-pool balance snapshot for `account_id`, anchored to the wallet's last
    /// observed chain tip.
    ///
    /// Sapling, Orchard, and Ironwood values come from `WalletRead::get_wallet_summary`; the
    /// transparent mature/immature split applies ZIP-213 coinbase maturity directly against
    /// the `transparent_received_outputs` rows so the figure stays consistent with what
    /// `shield_transparent_funds` will accept as input. Unconfirmed wallet-owned spends are
    /// excluded from the totals so callers do not double-count outpoints already consumed
    /// by a still-pending broadcast.
    ///
    /// `not_retryable` on unknown account or schema errors; `retryable` on transient I/O.
    async fn get_account_balance(
        &self,
        account_id: AccountId,
    ) -> Result<AccountBalanceRow, StorageError>;

    /// Truncates the wallet precisely to the supplied chain state, inserting that frontier
    /// as a checkpoint at its exact height and rewinding to it.
    ///
    /// Wraps `WalletWrite::truncate_to_chain_state`. It lands the wallet at exactly
    /// `chain_state.block_height()` (rather than snapping down to the nearest existing wallet
    /// checkpoint), so reorg recovery does not re-open a gap below the rewind target. The
    /// caller fetches the chain state for the rewind height from the chain source. The
    /// librustzcash backend bounds a valid rewind at the `COINBASE_MATURITY` window (100
    /// blocks); a deeper target fails `NotRetryable` and the caller surfaces it to the
    /// operator.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn truncate_to_chain_state(&self, chain_state: ChainState) -> Result<(), StorageError>;

    /// Returns every Sapling and Orchard note received in the inclusive height range
    /// `[from_height, to_height]`, regardless of current spent state.
    ///
    /// Powers the wallet's event stream: after a successful `scan_blocks` call the wallet
    /// looks up notes newly mined in the scanned range and emits one
    /// `ShieldedReceiveObserved` per row. Spent state is intentionally not filtered here
    /// so consumers see every receive that ever happened in the range, suitable for
    /// donation indexing and historical aggregation.
    ///
    /// `not_retryable` on missing account or schema errors; `retryable` on transient I/O.
    async fn received_shielded_notes_mined_in_range(
        &self,
        from_height: BlockHeight,
        to_height: BlockHeight,
    ) -> Result<Vec<ReceivedShieldedNoteRow>, StorageError>;

    /// Returns every Sapling, Orchard, and Ironwood note ever received by `account_id`, regardless
    /// of current spent state and regardless of when the note was mined.
    ///
    /// Powers operator-side replays that rebuild a downstream observation table from
    /// chain truth without coupling to the wallet's event stream. Each row carries the
    /// same provenance fields (`is_change`, `spent_our_inputs`) as the event-stream
    /// path, so the consumer can classify historical receives identically to live ones.
    ///
    /// `not_retryable` on unknown account or schema errors; `retryable` on transient I/O.
    async fn list_shielded_receives_for_account(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<ReceivedShieldedNoteRow>, StorageError>;

    /// Returns the decoded text memo for a shielded note the wallet owns.
    ///
    /// Returns `Ok(Some(text))` only when the memo encodes a UTF-8 string per
    /// ZIP 302's text-memo case (first byte `0x00..=0xF4`, trailing zeros stripped,
    /// remainder decodes as UTF-8). Empty memos (`0xF6` padding), arbitrary
    /// memos (`0xF5`, `0xFF`), future-reserved memos (`0xF7..=0xFE`), and notes
    /// the wallet does not know all return `Ok(None)`. Callers that surface
    /// memos to the public must only project the text variant; the other
    /// variants are opaque by construction and unsafe to render as strings.
    ///
    /// `tx_id` and `output_index` together identify the note; the implementation
    /// resolves across Sapling, Orchard, and Ironwood without requiring the caller to know
    /// which pool the note lives on.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn read_text_memo(
        &self,
        tx_id: TxId,
        output_index: u16,
    ) -> Result<Option<String>, StorageError>;

    /// Persists a fresh dispense reservation row, enforcing the
    /// "amount sum stays within spendable" invariant inside a single sqlite transaction.
    ///
    /// The storage layer rechecks `record.spendable_for_check_zat` against the live sum of
    /// active reservations for the same account before inserting; if the new row would
    /// push the total above `spendable_for_check_zat`, the call fails closed with
    /// [`StorageError::InsufficientFunds`].
    ///
    /// `request_id` is unique across active rows. A second call with the same
    /// `request_id` while a row exists returns
    /// [`StorageError::HoldRequestConflict`] so the wallet boundary can
    /// look up the existing reservation and surface it idempotently rather than
    /// double-reserve.
    ///
    /// `not_retryable` on `InsufficientFunds` and `HoldRequestConflict`;
    /// `retryable` on transient I/O.
    async fn create_hold(&self, record: HoldRecord) -> Result<(), StorageError>;

    /// Marks `hold_id` as released. The row stays in storage for audit; subsequent
    /// reads see `released_at_ms = Some(now)` and the reservation no longer contributes
    /// to `sum_active_dispense_reserved_zat`.
    ///
    /// Idempotent: a second call on the same identifier observes the prior
    /// `released_at_ms` and returns `Ok(())`. A row that was already finalized stays
    /// finalized; this method only updates `released_at_ms` when the row is still active.
    ///
    /// Returns [`StorageError::HoldNotFound`] when no row matches.
    ///
    /// `not_retryable` on `HoldNotFound`; `retryable` on transient I/O.
    async fn release_hold(&self, hold_id: HoldId, released_at_ms: u64) -> Result<(), StorageError>;

    /// Marks `hold_id` as finalized by a specific broadcast transaction.
    ///
    /// Idempotent: a second call with the same `tx_id` observes the prior
    /// `finalized_tx_id` and returns `Ok(())`. Calling with a different `tx_id` after a
    /// prior finalize is treated as a no-op as well, on the assumption that the caller
    /// is replaying a recovery path; the persisted `tx_id` is the authoritative one.
    ///
    /// Returns [`StorageError::HoldNotFound`] when no row matches.
    ///
    /// `not_retryable` on `HoldNotFound`; `retryable` on transient I/O.
    async fn finalize_hold(&self, hold_id: HoldId, tx_id: TxId) -> Result<(), StorageError>;

    /// Returns the persisted reservation row whose caller-supplied `request_id` matches,
    /// regardless of whether it is still active, finalized, or released.
    ///
    /// Powers idempotent reservation: when the wallet boundary observes a request id
    /// already recorded, it can return the prior reservation summary instead of trying
    /// to create a new row that would conflict.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn find_hold_by_request_id(
        &self,
        request_id: &IdempotencyKey,
    ) -> Result<Option<HoldRow>, StorageError>;

    /// Returns every active reservation row for `account_id`: rows with both
    /// `finalized_tx_id` and `released_at_ms` still `None`.
    ///
    /// Powers `Wallet::spendable_for_next_dispense` and the operator-facing view of
    /// what the wallet has currently locked.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn list_active_holds(&self, account_id: AccountId) -> Result<Vec<HoldRow>, StorageError>;

    /// Returns the sum of `amount_zat` across every active reservation row for
    /// `account_id`. Equivalent to summing the rows from
    /// [`Self::list_active_holds`] but avoids the per-row decode cost
    /// for the steady-state spendable-balance read path.
    ///
    /// `not_retryable` on schema errors; `retryable` on transient I/O.
    async fn sum_active_dispense_reserved_zat(
        &self,
        account_id: AccountId,
    ) -> Result<Zatoshis, StorageError>;
}
