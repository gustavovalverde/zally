//! `SQLite`-backed wallet storage.
//!
//! Every wallet-db call funnels through a single-threaded **wallet-db actor**.
//! One owned thread owns the [`WalletDb`] and the ledger [`rusqlite::Connection`];
//! callers send type-erased [`DbWork`] closures over a bounded `mpsc` channel and
//! await a `oneshot` reply. `rusqlite::Connection` is `!Sync`, and librustzcash
//! proposal construction can hold wallet-db access while proving. The actor makes
//! that serialization explicit, bounded, and observable.
//!
//! [`SqliteWalletStorage`] is a cheap [`Clone`] handle holding only the
//! channel sender; the actor lives until every clone is dropped.

use std::path::PathBuf;

use async_trait::async_trait;
use rand::rngs::OsRng;
use rusqlite::OptionalExtension as _;
use secrecy::{ExposeSecret as _, SecretVec};
use tokio::sync::{mpsc, oneshot};
use zally_core::{
    AccountId, BlockHeight, FailurePosture, IdempotencyKey, Network, NetworkParameters, TxId,
    Zatoshis,
};
use zally_keys::{KeyDerivationError, SeedMaterial, derive_ufvk};
use zcash_client_backend::data_api::chain::ChainState;
use zcash_client_backend::data_api::wallet::{
    ConfirmationsPolicy, SpendingKeys, input_selection::GreedyInputSelector,
};
use zcash_client_backend::data_api::{Account, AccountBirthday, WalletRead, WalletWrite};
use zcash_client_backend::fees::{DustOutputPolicy, StandardFeeRule, standard};
use zcash_client_backend::wallet::WalletTransparentOutput;
use zcash_client_sqlite::AccountUuid;
use zcash_client_sqlite::WalletDb;
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::wallet::init::WalletMigrator;
use zcash_keys::address::UnifiedAddress;
use zcash_keys::keys::UnifiedAddressRequest;
use zcash_protocol::value::Zatoshis as UpstreamZatoshis;
use zcash_transparent::address::Script;
use zcash_transparent::bundle::{OutPoint as UpstreamOutPoint, TxOut};

use crate::storage_error::StorageError;
use crate::wallet_storage::WalletStorage;

type Db = WalletDb<rusqlite::Connection, NetworkParameters, SystemClock, OsRng>;

const DEFAULT_ACCOUNT_NAME: &str = "primary";

/// Bounded capacity of the wallet-db actor's request queue.
///
/// Sized above the runtime's steady-state concurrency (one wallet-sync loop,
/// one dispense reaper, one donation reconciler, and a handful of in-flight
/// dispense and balance reads). Hitting the bound back-pressures the sender
/// instead of letting an unbounded queue silently grow.
const WALLET_DB_QUEUE_CAPACITY: usize = 256;

/// Work item sent to the wallet-db actor.
///
/// The closure owns its own reply channel and typed result; the actor just
/// calls it. This is the type-erasure that lets one `mpsc` channel carry every
/// `with_db`, `with_db_mut`, `with_ledger`, and `open_or_create` request
/// without a per-method message variant.
type DbWork = Box<dyn FnOnce(&mut WalletDbState, &SqliteWalletStorageOptions) + Send>;

/// State held on the actor thread.
///
/// `NotOpened` is the initial state; the first successful
/// [`WalletStorage::open_or_create`] transitions to `Opened`. Every other
/// request errors with [`StorageError::NotOpened`] while the state is
/// `NotOpened`, matching the prior lazy-open contract.
enum WalletDbState {
    NotOpened,
    Opened {
        db: Box<Db>,
        ledger: Box<rusqlite::Connection>,
    },
}

/// Options for [`SqliteWalletStorage`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SqliteWalletStorageOptions {
    /// Path at which the `SQLite` database is opened or created.
    pub db_path: PathBuf,
    /// Network bound to this storage instance.
    pub network: Network,
    /// Human-readable account name recorded during `create_account_for_seed`.
    pub account_name: String,
}

impl SqliteWalletStorageOptions {
    /// Storage options for a network-bound wallet. The account name defaults to `"primary"`.
    #[must_use]
    pub fn for_network(network: Network, db_path: PathBuf) -> Self {
        Self {
            db_path,
            network,
            account_name: DEFAULT_ACCOUNT_NAME.to_owned(),
        }
    }
}

/// `SQLite`-backed [`WalletStorage`] implementation.
///
/// Wraps [`zcash_client_sqlite::WalletDb`]. The wallet database is opened lazily;
/// the first [`SqliteWalletStorage::open_or_create`] call transitions the
/// actor's state from `NotOpened` to `Opened`. Every public method routes its
/// blocking sqlite work through the wallet-db actor described in the module
/// docs.
///
/// Cloning yields another handle to the same actor; the actor lives until all
/// clones are dropped.
#[derive(Clone)]
pub struct SqliteWalletStorage {
    options: SqliteWalletStorageOptions,
    request_tx: mpsc::Sender<DbWork>,
}

impl SqliteWalletStorage {
    /// Constructs a new storage handle and spawns the wallet-db actor. The
    /// database is not opened until [`SqliteWalletStorage::open_or_create`] is
    /// called (the actor starts unopened).
    ///
    /// Must be called from within a tokio runtime; the actor lives on the
    /// runtime's blocking pool until all handles are dropped.
    #[must_use]
    pub fn new(options: SqliteWalletStorageOptions) -> Self {
        let (request_tx, request_rx) = mpsc::channel(WALLET_DB_QUEUE_CAPACITY);
        let actor_options = options.clone();
        drop(tokio::task::spawn_blocking(move || {
            run_wallet_db_actor(&actor_options, request_rx);
        }));
        Self {
            options,
            request_tx,
        }
    }

    /// Current number of work items queued on the actor. A depth that stays
    /// near the queue capacity means the actor cannot keep up with incoming
    /// load.
    #[must_use]
    pub fn queue_depth(&self) -> usize {
        WALLET_DB_QUEUE_CAPACITY - self.request_tx.capacity()
    }

    async fn dispatch<F, T>(&self, run_on_actor: F) -> Result<T, StorageError>
    where
        F: FnOnce(&mut WalletDbState, &SqliteWalletStorageOptions) -> Result<T, StorageError>
            + Send
            + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let work: DbWork = Box::new(move |state, options| {
            let outcome = run_on_actor(state, options);
            // The receiver is dropped only if the caller's future was
            // cancelled mid-flight; ignore the send failure in that case so
            // the actor stays healthy for the next request.
            drop(reply_tx.send(outcome));
        });
        self.request_tx
            .send(work)
            .await
            .map_err(|_| StorageError::BlockingTaskFailed {
                reason: "wallet-db actor channel closed".to_owned(),
            })?;
        reply_rx
            .await
            .map_err(|_| StorageError::BlockingTaskFailed {
                reason: "wallet-db actor dropped the reply".to_owned(),
            })?
    }

    async fn with_ledger<F, T>(&self, work: F) -> Result<T, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, StorageError> + Send + 'static,
        T: Send + 'static,
    {
        self.dispatch(move |state, _options| match state {
            WalletDbState::NotOpened => Err(StorageError::NotOpened),
            WalletDbState::Opened { ledger, .. } => work(ledger.as_ref()),
        })
        .await
    }

    async fn with_db<F, T>(&self, work: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Db) -> Result<T, StorageError> + Send + 'static,
        T: Send + 'static,
    {
        self.dispatch(move |state, _options| match state {
            WalletDbState::NotOpened => Err(StorageError::NotOpened),
            WalletDbState::Opened { db, .. } => work(db.as_ref()),
        })
        .await
    }

    async fn with_db_mut<F, T>(&self, work: F) -> Result<T, StorageError>
    where
        F: FnOnce(&mut Db) -> Result<T, StorageError> + Send + 'static,
        T: Send + 'static,
    {
        self.dispatch(move |state, _options| match state {
            WalletDbState::NotOpened => Err(StorageError::NotOpened),
            WalletDbState::Opened { db, .. } => work(db.as_mut()),
        })
        .await
    }

    async fn with_db_and_ledger<F, T>(&self, work: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Db, &rusqlite::Connection) -> Result<T, StorageError> + Send + 'static,
        T: Send + 'static,
    {
        self.dispatch(move |state, _options| match state {
            WalletDbState::NotOpened => Err(StorageError::NotOpened),
            WalletDbState::Opened { db, ledger } => work(db.as_ref(), ledger.as_ref()),
        })
        .await
    }
}

fn run_wallet_db_actor(
    options: &SqliteWalletStorageOptions,
    mut request_rx: mpsc::Receiver<DbWork>,
) {
    let mut state = WalletDbState::NotOpened;
    while let Some(work) = request_rx.blocking_recv() {
        work(&mut state, options);
    }
}

/// Opens (or creates on first run) the underlying `WalletDb` and the Zally
/// ledger connection backing the `ext_zally_*` tables. Runs on the actor
/// thread; the file I/O and the schema migration happen here.
fn open_wallet_db(options: &SqliteWalletStorageOptions) -> Result<WalletDbState, StorageError> {
    if let Some(parent) = options.db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| StorageError::SqliteFailed {
            reason: format!("could not create database directory: {e}"),
            posture: FailurePosture::NotRetryable,
        })?;
    }
    let params = options.network.to_parameters();
    let mut db = WalletDb::for_path(&options.db_path, params, SystemClock, OsRng).map_err(|e| {
        StorageError::SqliteFailed {
            reason: e.to_string(),
            posture: FailurePosture::NotRetryable,
        }
    })?;
    WalletMigrator::new()
        .init_or_migrate(&mut db)
        .map_err(|e| StorageError::MigrationFailed {
            reason: e.to_string(),
        })?;

    let ledger =
        rusqlite::Connection::open(&options.db_path).map_err(|e| StorageError::SqliteFailed {
            reason: format!("ledger connection open failed: {e}"),
            posture: FailurePosture::NotRetryable,
        })?;
    ledger
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS ext_zally_idempotency (\
                 idempotency_key TEXT PRIMARY KEY NOT NULL,\
                 tx_id_bytes BLOB NOT NULL,\
                 recorded_at_unix INTEGER NOT NULL\
             ); \
             CREATE TABLE IF NOT EXISTS ext_zally_observed_tip (\
                 id INTEGER PRIMARY KEY CHECK (id = 0),\
                 tip_height INTEGER NOT NULL\
             ); \
             CREATE TABLE IF NOT EXISTS ext_zally_pending_broadcast_inputs (\
                 broadcast_tx_id BLOB NOT NULL,\
                 outpoint_tx_id BLOB NOT NULL,\
                 outpoint_index INTEGER NOT NULL,\
                 value_zat INTEGER NOT NULL,\
                 account_id BLOB NOT NULL,\
                 broadcast_at_ms INTEGER NOT NULL,\
                 broadcast_at_height INTEGER,\
                 PRIMARY KEY (broadcast_tx_id, outpoint_tx_id, outpoint_index)\
             ); \
             CREATE INDEX IF NOT EXISTS ix_ezpbi_by_account \
                 ON ext_zally_pending_broadcast_inputs(account_id); \
             CREATE INDEX IF NOT EXISTS ix_ezpbi_by_broadcast_at \
                 ON ext_zally_pending_broadcast_inputs(broadcast_at_ms);",
        )
        .map_err(|e| StorageError::SqliteFailed {
            reason: format!("ext_zally schema init failed: {e}"),
            posture: FailurePosture::NotRetryable,
        })?;

    Ok(WalletDbState::Opened {
        db: Box::new(db),
        ledger: Box::new(ledger),
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "async-trait expands the WalletStorage impl; contiguous methods preserve the trait boundary"
)]
#[async_trait]
impl WalletStorage for SqliteWalletStorage {
    async fn open_or_create(&self) -> Result<(), StorageError> {
        self.dispatch(|state, options| {
            if matches!(state, WalletDbState::Opened { .. }) {
                return Ok(());
            }
            let opened = open_wallet_db(options)?;
            *state = opened;
            Ok(())
        })
        .await
    }

    async fn create_account_for_seed(
        &self,
        seed: &SeedMaterial,
        prior_chain_state: ChainState,
    ) -> Result<AccountId, StorageError> {
        let account_name = self.options.account_name.clone();
        let seed_bytes = seed.expose_secret().to_vec();
        let secret = SecretVec::new(seed_bytes);

        self.with_db_mut(move |db| {
            let birthday_height = prior_chain_state.block_height() + 1;
            let account_birthday = AccountBirthday::from_parts(prior_chain_state, None);
            let (account, _usk) = db
                .import_account_hd(
                    &account_name,
                    &secret,
                    zip32::AccountId::ZERO,
                    &account_birthday,
                    None,
                )
                .map_err(|e| map_sqlite_error(&e))?;
            db.update_chain_tip(birthday_height)
                .map_err(|e| map_sqlite_error(&e))?;
            Ok(account_uuid_to_zally(account.id()))
        })
        .await
    }

    async fn find_account_for_seed(
        &self,
        seed: &SeedMaterial,
    ) -> Result<Option<AccountId>, StorageError> {
        let network = self.options.network;
        let ufvk = derive_ufvk(network, seed, zip32::AccountId::ZERO)
            .map_err(|e| map_derivation_error(&e))?;

        self.with_db(move |db| {
            let account = db
                .get_account_for_ufvk(&ufvk)
                .map_err(|e| map_sqlite_error(&e))?;
            Ok(account.map(|a| account_uuid_to_zally(a.id())))
        })
        .await
    }

    async fn derive_next_address(
        &self,
        account_id: AccountId,
    ) -> Result<UnifiedAddress, StorageError> {
        let sqlite_uuid = zally_to_account_uuid(account_id);

        // `SHIELDED` returns a Unified Address with Orchard + Sapling receivers and skips
        // transparent entirely; this keeps the call free of the transparent gap-limit
        // (default 10 unused addresses) so operators can derive an unbounded stream of
        // receive addresses. Callers that need a transparent receiver call
        // [`Self::derive_next_address_with_transparent`] instead and accept the gap-limit
        // constraint that comes with it.
        self.with_db_mut(move |db| {
            let outcome = db
                .get_next_available_address(sqlite_uuid, UnifiedAddressRequest::SHIELDED)
                .map_err(|e| map_sqlite_error(&e))?;
            let (address, _diversifier) = outcome.ok_or(StorageError::AccountNotFound)?;
            Ok(address)
        })
        .await
    }

    async fn derive_next_address_with_transparent(
        &self,
        account_id: AccountId,
    ) -> Result<UnifiedAddress, StorageError> {
        let sqlite_uuid = zally_to_account_uuid(account_id);

        // `AllAvailableKeys` resolves the request against the actual UFVK. P2PKH becomes
        // `Require` for Zally UFVKs with a transparent component, which routes upstream
        // into the gap-limit pre-generation path. The first call to this method
        // on a fresh wallet returns a UA with a transparent receiver; subsequent calls
        // fail with the gap-limit error until an on-chain transaction credits one of the
        // reserved transparent addresses.
        self.with_db_mut(move |db| {
            let outcome = db
                .get_next_available_address(sqlite_uuid, UnifiedAddressRequest::AllAvailableKeys)
                .map_err(|e| map_sqlite_error(&e))?;
            let (address, _diversifier) = outcome.ok_or(StorageError::AccountNotFound)?;
            Ok(address)
        })
        .await
    }

    async fn list_transparent_receivers(
        &self,
    ) -> Result<Vec<crate::wallet_storage::TransparentReceiverRow>, StorageError> {
        self.with_db(move |db| {
            let accounts = db
                .get_unified_full_viewing_keys()
                .map_err(|e| map_sqlite_error(&e))?;
            let mut receivers = Vec::new();
            for account_uuid in accounts.keys().copied() {
                let transparent_addresses = db
                    .get_transparent_receivers(account_uuid, true, true)
                    .map_err(|e| map_sqlite_error(&e))?;
                for address in transparent_addresses.into_keys() {
                    let script = Script::from(address.script());
                    receivers.push(crate::wallet_storage::TransparentReceiverRow::new(
                        account_uuid_to_zally(account_uuid),
                        script.0.0,
                    ));
                }
            }
            Ok(receivers)
        })
        .await
    }

    async fn record_transparent_utxos(
        &self,
        utxos: Vec<crate::wallet_storage::TransparentUtxoRow>,
    ) -> Result<u64, StorageError> {
        self.with_db_mut(move |db| {
            let mut recorded_count = 0_u64;
            for utxo in utxos {
                let output = transparent_utxo_row_to_output(utxo)?;
                db.put_received_transparent_utxo(&output)
                    .map_err(|e| map_sqlite_error(&e))?;
                recorded_count = recorded_count.saturating_add(1);
            }
            Ok(recorded_count)
        })
        .await
    }

    fn network(&self) -> Network {
        self.options.network
    }

    async fn scan_blocks(
        &self,
        request: crate::wallet_storage::ScanRequest,
    ) -> Result<crate::wallet_storage::ScanResult, StorageError> {
        let params = self.options.network.to_parameters();
        let from_height_proto: zcash_protocol::consensus::BlockHeight = request.from_height.into();
        let block_count = u64::try_from(request.blocks.len()).unwrap_or(u64::MAX);
        let limit = request.blocks.len().max(1);
        let source = InMemoryBlockSource {
            blocks: request.blocks,
        };
        let from_state = request.from_state;

        self.with_db_mut(move |db| {
            zcash_client_backend::data_api::chain::scan_cached_blocks(
                &params,
                &source,
                db,
                from_height_proto,
                &from_state,
                limit,
            )
            .map_err(|err| map_scan_error(&err))
            .map(|summary| {
                let scanned_to_u32 = u32::from(summary.scanned_range().end.saturating_sub(1));
                crate::wallet_storage::ScanResult {
                    scanned_to_height: BlockHeight::from(scanned_to_u32),
                    block_count,
                }
            })
        })
        .await
    }

    async fn truncate_to_height(
        &self,
        max_height: BlockHeight,
    ) -> Result<BlockHeight, StorageError> {
        let target = zcash_protocol::consensus::BlockHeight::from(max_height.as_u32());
        self.with_db_mut(move |db| {
            zcash_client_backend::data_api::WalletWrite::truncate_to_height(db, target)
                .map(|new_height| BlockHeight::from(u32::from(new_height)))
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("truncate_to_height failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })
        })
        .await
    }

    async fn update_chain_tip(&self, tip_height: BlockHeight) -> Result<(), StorageError> {
        let target = zcash_protocol::consensus::BlockHeight::from(tip_height.as_u32());
        self.with_db_mut(move |db| {
            zcash_client_backend::data_api::WalletWrite::update_chain_tip(db, target).map_err(
                |err| StorageError::SqliteFailed {
                    reason: format!("update_chain_tip failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                },
            )
        })
        .await
    }

    async fn fully_scanned_height(&self) -> Result<Option<BlockHeight>, StorageError> {
        self.with_db(move |db| {
            let summary = db
                .get_wallet_summary(
                    zcash_client_backend::data_api::wallet::ConfirmationsPolicy::default(),
                )
                .map_err(|e| map_sqlite_error(&e))?;
            Ok(summary.map(|s| BlockHeight::from(u32::from(s.fully_scanned_height()))))
        })
        .await
    }

    async fn wallet_birthday(&self) -> Result<Option<BlockHeight>, StorageError> {
        self.with_db(move |db| {
            let height = zcash_client_backend::data_api::WalletRead::get_wallet_birthday(db)
                .map_err(|e| map_sqlite_error(&e))?;
            Ok(height.map(|h| BlockHeight::from(u32::from(h))))
        })
        .await
    }

    async fn propose_payment(
        &self,
        request: crate::wallet_storage::ProposalPaymentRequest,
    ) -> Result<crate::wallet_storage::ProposalSummary, StorageError> {
        let params = self.options.network.to_parameters();
        let account_uuid = zally_to_account_uuid(request.account_id);
        let amount = UpstreamZatoshis::from_u64(request.amount_zat).map_err(|err| {
            StorageError::SqliteFailed {
                reason: format!(
                    "amount {} exceeds Zatoshis maximum: {err}",
                    request.amount_zat
                ),
                posture: FailurePosture::NotRetryable,
            }
        })?;
        let recipient = zcash_keys::address::Address::decode(&params, &request.recipient_encoded)
            .ok_or_else(|| StorageError::SqliteFailed {
            reason: format!("could not decode recipient '{}'", request.recipient_encoded),
            posture: FailurePosture::NotRetryable,
        })?;
        let memo_bytes = request
            .memo
            .as_deref()
            .map(zcash_protocol::memo::MemoBytes::from_bytes)
            .transpose()
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("memo too long: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;

        self.with_db_mut(move |db| {
            let proposal =
                zcash_client_backend::data_api::wallet::propose_standard_transfer_to_address::<
                    _,
                    _,
                    zcash_client_sqlite::error::SqliteClientError,
                >(
                    db,
                    &params,
                    zcash_client_backend::fees::StandardFeeRule::Zip317,
                    account_uuid,
                    zcash_client_backend::data_api::wallet::ConfirmationsPolicy::default(),
                    &recipient,
                    amount,
                    memo_bytes,
                    None,
                    zcash_protocol::ShieldedProtocol::Orchard,
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("propose_transfer failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;

            let first_step = proposal.steps().first();
            let balance = first_step.balance();
            let payment_count = first_step.transaction_request().payments().len();
            let target_height: zcash_protocol::consensus::BlockHeight =
                proposal.min_target_height().into();

            Ok(crate::wallet_storage::ProposalSummary {
                total_zat: balance.total().into(),
                fee_zat: balance.fee_required().into(),
                min_target_height: BlockHeight::from(u32::from(target_height)),
                output_count: payment_count,
            })
        })
        .await
    }

    async fn prepare_payment(
        &self,
        request: crate::wallet_storage::ProposalPaymentRequest,
        seed: &SeedMaterial,
    ) -> Result<Vec<crate::wallet_storage::PreparedTransaction>, StorageError> {
        let params = self.options.network.to_parameters();
        let account_uuid = zally_to_account_uuid(request.account_id);
        let amount = UpstreamZatoshis::from_u64(request.amount_zat).map_err(|err| {
            StorageError::SqliteFailed {
                reason: format!(
                    "amount {} exceeds Zatoshis maximum: {err}",
                    request.amount_zat
                ),
                posture: FailurePosture::NotRetryable,
            }
        })?;
        let recipient = zcash_keys::address::Address::decode(&params, &request.recipient_encoded)
            .ok_or_else(|| StorageError::SqliteFailed {
            reason: format!("could not decode recipient '{}'", request.recipient_encoded),
            posture: FailurePosture::NotRetryable,
        })?;
        let memo_bytes = request
            .memo
            .as_deref()
            .map(zcash_protocol::memo::MemoBytes::from_bytes)
            .transpose()
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("memo too long: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;
        let prover = zcash_proofs::prover::LocalTxProver::with_default_location()
            .ok_or(StorageError::ProverUnavailable)?;
        let seed_bytes = SecretVec::new(seed.expose_secret().to_vec());
        let network = self.options.network;
        let account_id = request.account_id;
        let excluded_outpoints = request.excluded_outpoints;

        self.with_db_mut(move |db| {
            let usk = zcash_keys::keys::UnifiedSpendingKey::from_seed(
                &params,
                seed_bytes.expose_secret(),
                zip32::AccountId::ZERO,
            )
            .map_err(|err| StorageError::KeyDerivationFailed {
                reason: format!("ZIP-32 derivation failed: {err}"),
            })?;
            let spending_keys = SpendingKeys::from_unified_spending_key(usk);

            let proposal =
                zcash_client_backend::data_api::wallet::propose_standard_transfer_to_address::<
                    _,
                    _,
                    zcash_client_sqlite::error::SqliteClientError,
                >(
                    db,
                    &params,
                    zcash_client_backend::fees::StandardFeeRule::Zip317,
                    account_uuid,
                    zcash_client_backend::data_api::wallet::ConfirmationsPolicy::default(),
                    &recipient,
                    amount,
                    memo_bytes,
                    None,
                    zcash_protocol::ShieldedProtocol::Orchard,
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("propose_transfer failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;

            check_proposal_against_excluded_outpoints(
                &proposal,
                &excluded_outpoints,
                network,
                account_id,
            )?;

            let txids = zcash_client_backend::data_api::wallet::create_proposed_transactions::<
                Db,
                NetworkParameters,
                zcash_client_sqlite::error::SqliteClientError,
                zcash_client_backend::fees::StandardFeeRule,
                std::convert::Infallible,
                zcash_client_sqlite::ReceivedNoteId,
            >(
                db,
                &params,
                &prover,
                &prover,
                &spending_keys,
                zcash_client_backend::wallet::OvkPolicy::Sender,
                &proposal,
            )
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("create_proposed_transactions failed: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;

            prepared_transactions_for_txids(db, &txids, "created")
        })
        .await
    }

    async fn shield_transparent_funds(
        &self,
        request: crate::wallet_storage::ShieldTransparentRequest,
        seed: &SeedMaterial,
    ) -> Result<Vec<crate::wallet_storage::PreparedTransaction>, StorageError> {
        let params = self.options.network.to_parameters();
        let account_uuid = zally_to_account_uuid(request.account_id);
        let shielding_threshold = UpstreamZatoshis::from_u64(request.shielding_threshold_zat)
            .map_err(|_| StorageError::TransparentOutputValueOutOfRange {
                value_zat: request.shielding_threshold_zat,
            })?;
        let prover = zcash_proofs::prover::LocalTxProver::with_default_location()
            .ok_or(StorageError::ProverUnavailable)?;
        let seed_bytes = SecretVec::new(seed.expose_secret().to_vec());
        let network = self.options.network;
        let account_id = request.account_id;
        let excluded_outpoints = request.excluded_outpoints;

        self.with_db_mut(move |db| {
            let usk = zcash_keys::keys::UnifiedSpendingKey::from_seed(
                &params,
                seed_bytes.expose_secret(),
                zip32::AccountId::ZERO,
            )
            .map_err(|err| StorageError::KeyDerivationFailed {
                reason: format!("ZIP-32 derivation failed: {err}"),
            })?;
            let spending_keys = SpendingKeys::from_unified_spending_key(usk);
            let from_addrs: Vec<_> = db
                .get_transparent_receivers(account_uuid, true, true)
                .map_err(|e| map_sqlite_error(&e))?
                .into_keys()
                .collect();
            let input_selector = GreedyInputSelector::<Db>::new();
            let change_strategy = standard::SingleOutputChangeStrategy::<Db>::new(
                StandardFeeRule::Zip317,
                None,
                zcash_protocol::ShieldedProtocol::Orchard,
                DustOutputPolicy::default(),
            );

            let proposal = zcash_client_backend::data_api::wallet::propose_shielding::<
                Db,
                NetworkParameters,
                _,
                _,
                std::convert::Infallible,
            >(
                db,
                &params,
                &input_selector,
                &change_strategy,
                shielding_threshold,
                &from_addrs,
                account_uuid,
                ConfirmationsPolicy::default(),
                zcash_client_backend::data_api::TransparentOutputFilter::All,
            )
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("propose_shielding failed: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;

            check_proposal_against_excluded_outpoints(
                &proposal,
                &excluded_outpoints,
                network,
                account_id,
            )?;

            let txids = zcash_client_backend::data_api::wallet::create_proposed_transactions::<
                Db,
                NetworkParameters,
                zcash_client_sqlite::error::SqliteClientError,
                StandardFeeRule,
                std::convert::Infallible,
                std::convert::Infallible,
            >(
                db,
                &params,
                &prover,
                &prover,
                &spending_keys,
                zcash_client_backend::wallet::OvkPolicy::Sender,
                &proposal,
            )
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("shield_transparent_funds create failed: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;

            prepared_transactions_for_txids(db, txids.iter(), "shielding")
        })
        .await
    }

    async fn create_pczt(
        &self,
        request: crate::wallet_storage::ProposalPaymentRequest,
    ) -> Result<Vec<u8>, StorageError> {
        let params = self.options.network.to_parameters();
        let account_uuid = zally_to_account_uuid(request.account_id);
        let amount = UpstreamZatoshis::from_u64(request.amount_zat).map_err(|err| {
            StorageError::SqliteFailed {
                reason: format!(
                    "amount {} exceeds Zatoshis maximum: {err}",
                    request.amount_zat
                ),
                posture: FailurePosture::NotRetryable,
            }
        })?;
        let recipient = zcash_keys::address::Address::decode(&params, &request.recipient_encoded)
            .ok_or_else(|| StorageError::SqliteFailed {
            reason: format!("could not decode recipient '{}'", request.recipient_encoded),
            posture: FailurePosture::NotRetryable,
        })?;
        let memo_bytes = request
            .memo
            .as_deref()
            .map(zcash_protocol::memo::MemoBytes::from_bytes)
            .transpose()
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("memo too long: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;

        self.with_db_mut(move |db| {
            let proposal =
                zcash_client_backend::data_api::wallet::propose_standard_transfer_to_address::<
                    _,
                    _,
                    zcash_client_sqlite::error::SqliteClientError,
                >(
                    db,
                    &params,
                    zcash_client_backend::fees::StandardFeeRule::Zip317,
                    account_uuid,
                    zcash_client_backend::data_api::wallet::ConfirmationsPolicy::default(),
                    &recipient,
                    amount,
                    memo_bytes,
                    None,
                    zcash_protocol::ShieldedProtocol::Orchard,
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("propose_transfer failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;

            let pczt = zcash_client_backend::data_api::wallet::create_pczt_from_proposal::<
                Db,
                NetworkParameters,
                std::convert::Infallible,
                zcash_client_backend::fees::StandardFeeRule,
                std::convert::Infallible,
                zcash_client_sqlite::ReceivedNoteId,
            >(
                db,
                &params,
                account_uuid,
                zcash_client_backend::wallet::OvkPolicy::Sender,
                &proposal,
            )
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("create_pczt_from_proposal failed: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;

            Ok(pczt.serialize())
        })
        .await
    }

    async fn extract_and_store_pczt(
        &self,
        pczt_bytes: Vec<u8>,
    ) -> Result<crate::wallet_storage::PreparedTransaction, StorageError> {
        let parsed = pczt::Pczt::parse(&pczt_bytes).map_err(|err| StorageError::SqliteFailed {
            reason: format!("pczt parse failed: {err:?}"),
            posture: FailurePosture::NotRetryable,
        })?;
        let prover = zcash_proofs::prover::LocalTxProver::with_default_location()
            .ok_or(StorageError::ProverUnavailable)?;
        let (spend_vk, output_vk) = prover.verifying_keys();

        self.with_db_mut(move |db| {
            let tx_id =
                zcash_client_backend::data_api::wallet::extract_and_store_transaction_from_pczt::<
                    Db,
                    zcash_client_sqlite::ReceivedNoteId,
                >(db, parsed, Some((&spend_vk, &output_vk)), None)
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("extract_and_store_transaction_from_pczt failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;

            let stored = zcash_client_backend::data_api::WalletRead::get_transaction(db, tx_id)
                .map_err(|e| map_sqlite_error(&e))?
                .ok_or_else(|| StorageError::SqliteFailed {
                    reason: format!("extracted tx {tx_id} not present in wallet store"),
                    posture: FailurePosture::NotRetryable,
                })?;
            let mut raw_bytes = Vec::new();
            stored
                .write(&mut raw_bytes)
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("transaction serialize failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            Ok(crate::wallet_storage::PreparedTransaction::new(
                zally_core::TxId::from_bytes(*tx_id.as_ref()),
                raw_bytes,
            ))
        })
        .await
    }

    async fn find_idempotent_submission(
        &self,
        key: &IdempotencyKey,
    ) -> Result<Option<TxId>, StorageError> {
        let key_str = key.as_str().to_owned();
        self.with_ledger(move |conn| {
            let outcome = conn
                .query_row(
                    "SELECT tx_id_bytes FROM ext_zally_idempotency WHERE idempotency_key = ?1",
                    [&key_str],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("ext_zally_idempotency lookup failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            outcome
                .map(|raw| -> Result<TxId, StorageError> {
                    let array: [u8; 32] =
                        raw.try_into()
                            .map_err(|raw: Vec<u8>| StorageError::SqliteFailed {
                                reason: format!(
                                    "ext_zally_idempotency tx_id_bytes had wrong length: {}",
                                    raw.len()
                                ),
                                posture: FailurePosture::NotRetryable,
                            })?;
                    Ok(TxId::from_bytes(array))
                })
                .transpose()
        })
        .await
    }

    async fn record_idempotent_submission(
        &self,
        key: IdempotencyKey,
        tx_id: TxId,
    ) -> Result<(), StorageError> {
        let key_str = key.as_str().to_owned();
        let tx_bytes = tx_id.as_bytes().to_vec();
        let recorded_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_i64, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        self.with_ledger(move |conn| {
            let prior = conn
                .query_row(
                    "SELECT tx_id_bytes FROM ext_zally_idempotency WHERE idempotency_key = ?1",
                    [&key_str],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("ext_zally_idempotency lookup failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            if let Some(prior_bytes) = prior {
                return if prior_bytes == tx_bytes {
                    Ok(())
                } else {
                    Err(StorageError::IdempotencyKeyConflict)
                };
            }
            conn.execute(
                "INSERT INTO ext_zally_idempotency \
                     (idempotency_key, tx_id_bytes, recorded_at_unix) \
                  VALUES (?1, ?2, ?3)",
                rusqlite::params![&key_str, &tx_bytes, recorded_at_unix],
            )
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("ext_zally_idempotency insert failed: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;
            Ok(())
        })
        .await
    }

    async fn wallet_tx_ids_mined_in_range(
        &self,
        start: BlockHeight,
        end: BlockHeight,
    ) -> Result<Vec<(TxId, BlockHeight)>, StorageError> {
        let start_h = i64::from(start.as_u32());
        let end_h = i64::from(end.as_u32());
        self.with_ledger(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT txid, mined_height FROM transactions \
                     WHERE mined_height BETWEEN ?1 AND ?2 \
                     ORDER BY mined_height ASC, id_tx ASC",
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("transactions range query prepare failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            let rows = stmt
                .query_map([start_h, end_h], |row| {
                    let txid_bytes: Vec<u8> = row.get(0)?;
                    let mined_height: i64 = row.get(1)?;
                    Ok((txid_bytes, mined_height))
                })
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("transactions range query failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            let mut entries = Vec::new();
            for row in rows {
                let (txid_bytes, mined_height) = row.map_err(|err| StorageError::SqliteFailed {
                    reason: format!("transactions row decode failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
                let array: [u8; 32] =
                    txid_bytes
                        .try_into()
                        .map_err(|raw: Vec<u8>| StorageError::SqliteFailed {
                            reason: format!(
                                "transactions.txid had wrong byte length: {}",
                                raw.len()
                            ),
                            posture: FailurePosture::NotRetryable,
                        })?;
                let height =
                    u32::try_from(mined_height).map_err(|_| StorageError::SqliteFailed {
                        reason: format!(
                            "transactions.mined_height out of u32 range: {mined_height}"
                        ),
                        posture: FailurePosture::NotRetryable,
                    })?;
                entries.push((TxId::from_bytes(array), BlockHeight::from(height)));
            }
            Ok(entries)
        })
        .await
    }

    async fn find_observed_tip(&self) -> Result<Option<BlockHeight>, StorageError> {
        self.with_ledger(move |conn| {
            let outcome = conn
                .query_row(
                    "SELECT tip_height FROM ext_zally_observed_tip WHERE id = 0",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("ext_zally_observed_tip lookup failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            outcome
                .map(|raw| {
                    u32::try_from(raw).map(BlockHeight::from).map_err(|_| {
                        StorageError::SqliteFailed {
                            reason: format!("ext_zally_observed_tip.tip_height out of u32: {raw}"),
                            posture: FailurePosture::NotRetryable,
                        }
                    })
                })
                .transpose()
        })
        .await
    }

    async fn record_observed_tip(&self, new_tip: BlockHeight) -> Result<(), StorageError> {
        let new_tip_i64 = i64::from(new_tip.as_u32());
        self.with_ledger(move |conn| {
            conn.execute(
                "INSERT INTO ext_zally_observed_tip (id, tip_height) VALUES (0, ?1) \
                 ON CONFLICT(id) DO UPDATE SET tip_height = excluded.tip_height",
                [new_tip_i64],
            )
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("ext_zally_observed_tip upsert failed: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;
            Ok(())
        })
        .await
    }

    async fn record_pending_broadcast_inputs(
        &self,
        record: crate::wallet_storage::PendingBroadcastRecord,
    ) -> Result<(), StorageError> {
        let account_uuid_bytes = record.account_id.as_uuid().as_bytes().to_vec();
        let broadcast_tx_bytes = record.broadcast_tx_id.as_bytes().to_vec();
        let broadcast_at_ms_i64 = clamp_unsigned_to_i64(record.broadcast_at_ms);
        let broadcast_at_height_i64 = record.broadcast_at_height.map(|h| i64::from(h.as_u32()));
        let inputs = record.inputs;

        self.with_ledger(move |conn| {
            for (outpoint, value_zat) in inputs {
                let outpoint_tx_bytes = outpoint.tx_id.as_bytes().to_vec();
                let value_zat_i64 = clamp_unsigned_to_i64(value_zat.as_u64());
                conn.execute(
                    "INSERT INTO ext_zally_pending_broadcast_inputs \
                         (broadcast_tx_id, outpoint_tx_id, outpoint_index, value_zat, \
                          account_id, broadcast_at_ms, broadcast_at_height) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                     ON CONFLICT(broadcast_tx_id, outpoint_tx_id, outpoint_index) DO UPDATE \
                     SET value_zat = excluded.value_zat, \
                         account_id = excluded.account_id, \
                         broadcast_at_ms = excluded.broadcast_at_ms, \
                         broadcast_at_height = excluded.broadcast_at_height",
                    rusqlite::params![
                        &broadcast_tx_bytes,
                        &outpoint_tx_bytes,
                        i64::from(outpoint.output_index),
                        value_zat_i64,
                        &account_uuid_bytes,
                        broadcast_at_ms_i64,
                        broadcast_at_height_i64,
                    ],
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("ext_zally_pending_broadcast_inputs insert failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            }
            Ok(())
        })
        .await
    }

    async fn list_pending_broadcast_inputs(
        &self,
        account_id: AccountId,
        after_at_ms: u64,
    ) -> Result<Vec<crate::PendingBroadcastInputRow>, StorageError> {
        let account_uuid_bytes = account_id.as_uuid().as_bytes().to_vec();
        let after_at_ms_i64 = clamp_unsigned_to_i64(after_at_ms);

        self.with_ledger(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT broadcast_tx_id, outpoint_tx_id, outpoint_index, value_zat, \
                            broadcast_at_ms, broadcast_at_height \
                     FROM ext_zally_pending_broadcast_inputs \
                     WHERE account_id = ?1 AND broadcast_at_ms >= ?2 \
                     ORDER BY broadcast_at_ms ASC, broadcast_tx_id ASC, outpoint_index ASC",
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("pending-broadcast-inputs prepare failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            let mapped = stmt
                .query_map(
                    rusqlite::params![&account_uuid_bytes, after_at_ms_i64],
                    |row| {
                        let broadcast_tx_bytes: Vec<u8> = row.get(0)?;
                        let outpoint_tx_bytes: Vec<u8> = row.get(1)?;
                        let outpoint_index: i64 = row.get(2)?;
                        let value_zat: i64 = row.get(3)?;
                        let broadcast_at_ms: i64 = row.get(4)?;
                        let broadcast_at_height: Option<i64> = row.get(5)?;
                        Ok((
                            broadcast_tx_bytes,
                            outpoint_tx_bytes,
                            outpoint_index,
                            value_zat,
                            broadcast_at_ms,
                            broadcast_at_height,
                        ))
                    },
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("pending-broadcast-inputs query failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            let mut rows = Vec::new();
            for raw in mapped {
                let (
                    broadcast_tx_bytes,
                    outpoint_tx_bytes,
                    outpoint_index_raw,
                    value_zat_raw,
                    broadcast_at_ms_raw,
                    broadcast_at_height_raw,
                ) = raw.map_err(|err| StorageError::SqliteFailed {
                    reason: format!("pending-broadcast-inputs row decode failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
                let broadcast_tx_id = decode_txid_bytes(broadcast_tx_bytes, "broadcast_tx_id")?;
                let outpoint_tx_id = decode_txid_bytes(outpoint_tx_bytes, "outpoint_tx_id")?;
                let outpoint_index =
                    u32::try_from(outpoint_index_raw).map_err(|_| StorageError::SqliteFailed {
                        reason: format!(
                            "ext_zally_pending_broadcast_inputs.outpoint_index out of u32: \
                             {outpoint_index_raw}"
                        ),
                        posture: FailurePosture::NotRetryable,
                    })?;
                let value_zat_u64 =
                    u64::try_from(value_zat_raw).map_err(|_| StorageError::SqliteFailed {
                        reason: format!(
                            "ext_zally_pending_broadcast_inputs.value_zat out of u64: \
                             {value_zat_raw}"
                        ),
                        posture: FailurePosture::NotRetryable,
                    })?;
                let value_zat = Zatoshis::try_from(value_zat_u64).map_err(|_| {
                    StorageError::RowValueOutOfRange {
                        column: "ext_zally_pending_broadcast_inputs.value_zat",
                        raw: value_zat_u64.to_string(),
                    }
                })?;
                let broadcast_at_ms =
                    u64::try_from(broadcast_at_ms_raw).map_err(|_| StorageError::SqliteFailed {
                        reason: format!(
                            "ext_zally_pending_broadcast_inputs.broadcast_at_ms out of u64: \
                             {broadcast_at_ms_raw}"
                        ),
                        posture: FailurePosture::NotRetryable,
                    })?;
                let broadcast_at_height = broadcast_at_height_raw
                    .map(|raw_height| {
                        u32::try_from(raw_height)
                            .map(BlockHeight::from)
                            .map_err(|_| StorageError::SqliteFailed {
                                reason: format!(
                                    "ext_zally_pending_broadcast_inputs.broadcast_at_height \
                                     out of u32: {raw_height}"
                                ),
                                posture: FailurePosture::NotRetryable,
                            })
                    })
                    .transpose()?;
                rows.push(crate::PendingBroadcastInputRow {
                    broadcast_tx_id,
                    outpoint: zally_core::OutPoint::new(outpoint_tx_id, outpoint_index),
                    value_zat,
                    broadcast_at_ms,
                    broadcast_at_height,
                });
            }
            Ok(rows)
        })
        .await
    }

    async fn clear_pending_broadcast_inputs_for_mined(
        &self,
        tx_ids: &[TxId],
    ) -> Result<u64, StorageError> {
        if tx_ids.is_empty() {
            return Ok(0);
        }
        let tx_id_blobs: Vec<Vec<u8>> = tx_ids.iter().map(|t| t.as_bytes().to_vec()).collect();
        self.with_ledger(move |conn| {
            let mut removed = 0_u64;
            for blob in tx_id_blobs {
                let count = conn
                    .execute(
                        "DELETE FROM ext_zally_pending_broadcast_inputs WHERE broadcast_tx_id = ?1",
                        [&blob],
                    )
                    .map_err(|err| StorageError::SqliteFailed {
                        reason: format!(
                            "ext_zally_pending_broadcast_inputs cleanup-for-mined failed: {err}"
                        ),
                        posture: FailurePosture::NotRetryable,
                    })?;
                removed = removed.saturating_add(u64::try_from(count).unwrap_or(0));
            }
            Ok(removed)
        })
        .await
    }

    async fn clear_expired_pending_broadcast_inputs(
        &self,
        before_at_ms: u64,
    ) -> Result<u64, StorageError> {
        let before_at_ms_i64 = clamp_unsigned_to_i64(before_at_ms);
        self.with_ledger(move |conn| {
            let count = conn
                .execute(
                    "DELETE FROM ext_zally_pending_broadcast_inputs WHERE broadcast_at_ms < ?1",
                    [before_at_ms_i64],
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!(
                        "ext_zally_pending_broadcast_inputs cleanup-expired failed: {err}"
                    ),
                    posture: FailurePosture::NotRetryable,
                })?;
            Ok(u64::try_from(count).unwrap_or(0))
        })
        .await
    }

    async fn find_outpoint_values(
        &self,
        outpoints: &[zally_core::OutPoint],
    ) -> Result<std::collections::HashMap<zally_core::OutPoint, Zatoshis>, StorageError> {
        if outpoints.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let queries: Vec<(Vec<u8>, i64, zally_core::OutPoint)> = outpoints
            .iter()
            .map(|op| {
                (
                    op.tx_id.as_bytes().to_vec(),
                    i64::from(op.output_index),
                    *op,
                )
            })
            .collect();

        self.with_ledger(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT tro.value_zat \
                     FROM transparent_received_outputs tro \
                     JOIN transactions t ON t.id_tx = tro.transaction_id \
                     WHERE t.txid = ?1 AND tro.output_index = ?2",
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("find_outpoint_values prepare failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            let mut resolved = std::collections::HashMap::new();
            for (txid_bytes, output_index_i64, outpoint) in queries {
                let value_zat: Option<i64> = stmt
                    .query_row(rusqlite::params![&txid_bytes, output_index_i64], |row| {
                        row.get(0)
                    })
                    .optional()
                    .map_err(|err| StorageError::SqliteFailed {
                        reason: format!("find_outpoint_values query failed: {err}"),
                        posture: FailurePosture::NotRetryable,
                    })?;
                if let Some(raw) = value_zat {
                    let zat = decode_row_zatoshis(raw, "transparent_received_outputs.value_zat")?;
                    resolved.insert(outpoint, zat);
                }
            }
            Ok(resolved)
        })
        .await
    }

    async fn list_exposed_addresses(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<crate::ExposedAddressRow>, StorageError> {
        let account_uuid = zally_to_account_uuid(account_id);
        let account_uuid_bytes = account_id.as_uuid().as_bytes().to_vec();
        let params = self.options.network.to_parameters();

        self.with_db_and_ledger(move |db, ledger| {
            if WalletRead::get_account(db, account_uuid)
                .map_err(|e| map_sqlite_error(&e))?
                .is_none()
            {
                return Err(StorageError::AccountNotFound);
            }

            let mut stmt = ledger.prepare(EXPOSED_ADDRESSES_SQL).map_err(|err| {
                StorageError::SqliteFailed {
                    reason: format!("list_exposed_addresses prepare failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                }
            })?;
            let mapped = stmt
                .query_map([&account_uuid_bytes], |row| {
                    let address: String = row.get(0)?;
                    let diversifier_index_be: Vec<u8> = row.get(1)?;
                    let exposed_at_height: Option<i64> = row.get(2)?;
                    Ok((address, diversifier_index_be, exposed_at_height))
                })
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("list_exposed_addresses query failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;

            let mut rows = Vec::new();
            for raw in mapped {
                let (address_str, di_be_bytes, exposed_at_height_raw) =
                    raw.map_err(|err| StorageError::SqliteFailed {
                        reason: format!("list_exposed_addresses row decode failed: {err}"),
                        posture: FailurePosture::NotRetryable,
                    })?;
                let address = zcash_keys::address::Address::decode(&params, &address_str)
                    .ok_or_else(|| StorageError::SqliteFailed {
                        reason: format!("undecodable address in store: {address_str}"),
                        posture: FailurePosture::NotRetryable,
                    })?;
                let zcash_keys::address::Address::Unified(unified_address) = address else {
                    continue;
                };
                let diversifier_index = decode_diversifier_index_be(di_be_bytes)?;
                let exposed_at_height = exposed_at_height_raw
                    .map(|raw_height| {
                        u32::try_from(raw_height)
                            .map(BlockHeight::from)
                            .map_err(|_| StorageError::SqliteFailed {
                                reason: format!(
                                    "addresses.exposed_at_height out of u32: {raw_height}"
                                ),
                                posture: FailurePosture::NotRetryable,
                            })
                    })
                    .transpose()?;
                let has_transparent_receiver = unified_address.transparent().is_some();
                rows.push(crate::ExposedAddressRow {
                    unified_address,
                    diversifier_index,
                    has_transparent_receiver,
                    exposed_at_height,
                });
            }
            Ok(rows)
        })
        .await
    }

    async fn get_account_balance(
        &self,
        account_id: AccountId,
    ) -> Result<crate::AccountBalanceRow, StorageError> {
        let account_uuid = zally_to_account_uuid(account_id);
        let account_uuid_bytes = account_id.as_uuid().as_bytes().to_vec();

        self.with_db_and_ledger(move |db, ledger| {
            if WalletRead::get_account(db, account_uuid)
                .map_err(|e| map_sqlite_error(&e))?
                .is_none()
            {
                return Err(StorageError::AccountNotFound);
            }

            let summary = db
                .get_wallet_summary(ConfirmationsPolicy::default())
                .map_err(|e| map_sqlite_error(&e))?;
            let (sapling_zat, orchard_zat, transparent_mature_zat) = summary
                .as_ref()
                .and_then(|s| s.account_balances().get(&account_uuid))
                .map_or(
                    (Zatoshis::zero(), Zatoshis::zero(), Zatoshis::zero()),
                    |balance| {
                        (
                            upstream_to_zally_zatoshis(balance.sapling_balance().spendable_value()),
                            upstream_to_zally_zatoshis(balance.orchard_balance().spendable_value()),
                            upstream_to_zally_zatoshis(
                                balance.unshielded_balance().spendable_value(),
                            ),
                        )
                    },
                );

            let observed_tip = find_observed_tip_on(ledger)?;
            // ZIP-213 maturity uses `chain_tip + 1` per `zcash_client_backend`'s convention so
            // an output mined at H is considered mature once the chain tip reaches H+99
            // (confirmations = 100). On a fresh wallet with no observed tip the maturity
            // check is moot because no coinbase has been recorded yet.
            let target_height_i64 =
                i64::from(observed_tip.map_or(0_u32, |h| h.as_u32().saturating_add(1)));

            let immature_raw = ledger
                .query_row(
                    IMMATURE_COINBASE_AGGREGATE_SQL,
                    rusqlite::params![&account_uuid_bytes, target_height_i64],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("immature coinbase aggregate failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            let transparent_immature_zat = decode_row_zatoshis(
                immature_raw,
                "transparent_received_outputs.value_zat (immature coinbase)",
            )?;

            Ok(crate::AccountBalanceRow {
                sapling_zat,
                orchard_zat,
                transparent_mature_zat,
                transparent_immature_zat,
                as_of_height: observed_tip,
            })
        })
        .await
    }

    async fn list_unspent_shielded_notes(
        &self,
        account_id: AccountId,
        target_height: BlockHeight,
    ) -> Result<Vec<crate::wallet_storage::UnspentShieldedNoteRow>, StorageError> {
        let account_uuid = zally_to_account_uuid(account_id);
        let target = zcash_client_backend::data_api::wallet::TargetHeight::from(
            zcash_protocol::consensus::BlockHeight::from(target_height.as_u32()),
        );
        self.with_db(move |db| {
            let received = zcash_client_backend::data_api::InputSource::select_unspent_notes(
                db,
                account_uuid,
                &[
                    zcash_protocol::ShieldedProtocol::Sapling,
                    zcash_protocol::ShieldedProtocol::Orchard,
                ],
                target,
                &[],
            )
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("select_unspent_notes failed: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;

            let mut rows = Vec::new();
            for note in received.sapling() {
                let Some(mined_height) = note.mined_height() else {
                    continue;
                };
                rows.push(crate::wallet_storage::UnspentShieldedNoteRow {
                    protocol: zcash_protocol::ShieldedProtocol::Sapling,
                    value_zat: note.note().value().inner(),
                    tx_id: zally_core::TxId::from_bytes(*note.txid().as_ref()),
                    output_index: u32::from(note.output_index()),
                    mined_height: BlockHeight::from(u32::from(mined_height)),
                });
            }
            for note in received.orchard() {
                let Some(mined_height) = note.mined_height() else {
                    continue;
                };
                rows.push(crate::wallet_storage::UnspentShieldedNoteRow {
                    protocol: zcash_protocol::ShieldedProtocol::Orchard,
                    value_zat: note.note().value().inner(),
                    tx_id: zally_core::TxId::from_bytes(*note.txid().as_ref()),
                    output_index: u32::from(note.output_index()),
                    mined_height: BlockHeight::from(u32::from(mined_height)),
                });
            }
            Ok(rows)
        })
        .await
    }

    async fn received_shielded_notes_mined_in_range(
        &self,
        from_height: BlockHeight,
        to_height: BlockHeight,
    ) -> Result<Vec<crate::wallet_storage::ReceivedShieldedNoteRow>, StorageError> {
        let from_h = i64::from(from_height.as_u32());
        let to_h = i64::from(to_height.as_u32());
        self.with_ledger(move |conn| {
            let mut stmt = conn
                .prepare(RECEIVED_SHIELDED_NOTES_IN_RANGE_SQL)
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("received_shielded_notes prepare failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            let mapped = stmt
                .query_map([from_h, to_h], decode_received_shielded_note_row)
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("received_shielded_notes query failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            collect_received_shielded_note_rows(mapped)
        })
        .await
    }

    async fn list_shielded_receives_for_account(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<crate::wallet_storage::ReceivedShieldedNoteRow>, StorageError> {
        let account_uuid_bytes = account_id.as_uuid().as_bytes().to_vec();
        self.with_ledger(move |conn| {
            let mut stmt = conn
                .prepare(RECEIVED_SHIELDED_NOTES_FOR_ACCOUNT_SQL)
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("list_shielded_receives prepare failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            let mapped = stmt
                .query_map([&account_uuid_bytes], decode_received_shielded_note_row)
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("list_shielded_receives query failed: {err}"),
                    posture: FailurePosture::NotRetryable,
                })?;
            collect_received_shielded_note_rows(mapped)
        })
        .await
    }
}

/// Previously-exposed Unified Addresses for one account, in derivation order.
///
/// Bound to one account by uuid (`?1`); returns the encoded address string, the
/// diversifier-index blob (big-endian), and the optional exposure height. Filtering to UA
/// shape, decoding the diversifier index, and projecting to [`ExposedAddressRow`] happens
/// in Rust so the SQL stays portable across `zcash_client_sqlite` migrations.
///
/// [`ExposedAddressRow`]: crate::ExposedAddressRow
const EXPOSED_ADDRESSES_SQL: &str = "\
    SELECT a.address, a.diversifier_index_be, a.exposed_at_height \
    FROM addresses a \
    JOIN accounts ac ON ac.id = a.account_id \
    WHERE ac.uuid = ?1 \
      AND a.exposed_at_height IS NOT NULL \
    ORDER BY a.exposed_at_height ASC, a.diversifier_index_be ASC";

/// Aggregate of unmatured wallet-owned coinbase transparent value at one target height.
///
/// Bound to one account by uuid (`?1`) and one target height (`?2`, conventionally
/// `observed_tip + 1` to match `zcash_client_backend`'s `chain_tip + 1` convention).
/// Returns the sum of `value_zat` across `transparent_received_outputs` whose producing
/// transaction is coinbase and whose ZIP-213 100-block maturity window has not yet closed
/// at `?2`. Outputs already consumed by a wallet-owned spending transaction are excluded
/// regardless of whether that spend has confirmed, so a still-unconfirmed shielding tx that
/// already spent the immature coinbase does not double-count its value: the `mature`
/// half (from `unshielded_balance().spendable_value()`) excludes the same outputs.
const IMMATURE_COINBASE_AGGREGATE_SQL: &str = "\
    SELECT COALESCE(SUM(tro.value_zat), 0) \
    FROM transparent_received_outputs tro \
    JOIN transactions t ON t.id_tx = tro.transaction_id \
    JOIN accounts a ON a.id = tro.account_id \
    WHERE a.uuid = ?1 \
      AND t.mined_height IS NOT NULL \
      AND IFNULL(t.tx_index, 1) = 0 \
      AND (CAST(?2 AS INTEGER) - t.mined_height) < 100 \
      AND tro.id NOT IN ( \
        SELECT s.transparent_received_output_id \
        FROM transparent_received_output_spends s \
      )";

/// Per-pool receive query bounded by a mined-height window.
///
/// Joins the change flag from the per-pool received-note table, the block-header timestamp
/// from the `blocks` table, and a transaction-level flag indicating whether the producing
/// transaction spent any input the receiving account owned.
const RECEIVED_SHIELDED_NOTES_IN_RANGE_SQL: &str = "\
    SELECT a.uuid, t.txid, srn.output_index, srn.value, t.mined_height, 'sapling' AS pool, \
           b.time * 1000 AS block_timestamp_ms, \
           srn.is_change, \
           CASE WHEN EXISTS (SELECT 1 FROM sapling_received_note_spends s WHERE s.transaction_id = t.id_tx) \
                  OR EXISTS (SELECT 1 FROM orchard_received_note_spends o WHERE o.transaction_id = t.id_tx) \
                  OR EXISTS (SELECT 1 FROM transparent_received_output_spends p WHERE p.transaction_id = t.id_tx) \
                THEN 1 ELSE 0 END AS spent_our_inputs \
    FROM sapling_received_notes srn \
    JOIN transactions t ON srn.transaction_id = t.id_tx \
    JOIN accounts a ON srn.account_id = a.id \
    LEFT JOIN blocks b ON b.height = t.mined_height \
    WHERE t.mined_height BETWEEN ?1 AND ?2 \
    UNION ALL \
    SELECT a.uuid, t.txid, orn.action_index, orn.value, t.mined_height, 'orchard' AS pool, \
           b.time * 1000 AS block_timestamp_ms, \
           orn.is_change, \
           CASE WHEN EXISTS (SELECT 1 FROM sapling_received_note_spends s WHERE s.transaction_id = t.id_tx) \
                  OR EXISTS (SELECT 1 FROM orchard_received_note_spends o WHERE o.transaction_id = t.id_tx) \
                  OR EXISTS (SELECT 1 FROM transparent_received_output_spends p WHERE p.transaction_id = t.id_tx) \
                THEN 1 ELSE 0 END AS spent_our_inputs \
    FROM orchard_received_notes orn \
    JOIN transactions t ON orn.transaction_id = t.id_tx \
    JOIN accounts a ON orn.account_id = a.id \
    LEFT JOIN blocks b ON b.height = t.mined_height \
    WHERE t.mined_height BETWEEN ?1 AND ?2 \
    ORDER BY mined_height ASC, txid ASC, output_index ASC, pool ASC";

/// Per-account full-history receive query.
///
/// Returns every Sapling and Orchard receive ever observed for one account with the same
/// provenance fields and block-header timestamp the in-range query carries. Powers
/// historical replays that classify every receive at boot, independent of the wallet's
/// event stream.
const RECEIVED_SHIELDED_NOTES_FOR_ACCOUNT_SQL: &str = "\
    SELECT a.uuid, t.txid, srn.output_index, srn.value, t.mined_height, 'sapling' AS pool, \
           b.time * 1000 AS block_timestamp_ms, \
           srn.is_change, \
           CASE WHEN EXISTS (SELECT 1 FROM sapling_received_note_spends s WHERE s.transaction_id = t.id_tx) \
                  OR EXISTS (SELECT 1 FROM orchard_received_note_spends o WHERE o.transaction_id = t.id_tx) \
                  OR EXISTS (SELECT 1 FROM transparent_received_output_spends p WHERE p.transaction_id = t.id_tx) \
                THEN 1 ELSE 0 END AS spent_our_inputs \
    FROM sapling_received_notes srn \
    JOIN transactions t ON srn.transaction_id = t.id_tx \
    JOIN accounts a ON srn.account_id = a.id \
    LEFT JOIN blocks b ON b.height = t.mined_height \
    WHERE a.uuid = ?1 AND t.mined_height IS NOT NULL \
    UNION ALL \
    SELECT a.uuid, t.txid, orn.action_index, orn.value, t.mined_height, 'orchard' AS pool, \
           b.time * 1000 AS block_timestamp_ms, \
           orn.is_change, \
           CASE WHEN EXISTS (SELECT 1 FROM sapling_received_note_spends s WHERE s.transaction_id = t.id_tx) \
                  OR EXISTS (SELECT 1 FROM orchard_received_note_spends o WHERE o.transaction_id = t.id_tx) \
                  OR EXISTS (SELECT 1 FROM transparent_received_output_spends p WHERE p.transaction_id = t.id_tx) \
                THEN 1 ELSE 0 END AS spent_our_inputs \
    FROM orchard_received_notes orn \
    JOIN transactions t ON orn.transaction_id = t.id_tx \
    JOIN accounts a ON orn.account_id = a.id \
    LEFT JOIN blocks b ON b.height = t.mined_height \
    WHERE a.uuid = ?1 AND t.mined_height IS NOT NULL \
    ORDER BY mined_height ASC, txid ASC, output_index ASC, pool ASC";

type ReceivedShieldedNoteRowRaw = (
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
    i64,
    String,
    Option<i64>,
    i64,
    i64,
);

fn decode_received_shielded_note_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ReceivedShieldedNoteRowRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}

fn collect_received_shielded_note_rows(
    mapped: rusqlite::MappedRows<
        '_,
        impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<ReceivedShieldedNoteRowRaw>,
    >,
) -> Result<Vec<crate::wallet_storage::ReceivedShieldedNoteRow>, StorageError> {
    let mut rows = Vec::new();
    for raw in mapped {
        let (
            account_uuid_bytes,
            txid_bytes,
            output_index,
            value_zat,
            mined_height,
            pool,
            block_timestamp_ms,
            is_change,
            spent_our_inputs,
        ) = raw.map_err(|err| StorageError::SqliteFailed {
            reason: format!("received_shielded_notes row decode failed: {err}"),
            posture: FailurePosture::NotRetryable,
        })?;
        let txid_array: [u8; 32] =
            txid_bytes
                .try_into()
                .map_err(|raw: Vec<u8>| StorageError::SqliteFailed {
                    reason: format!("transactions.txid had wrong byte length: {}", raw.len()),
                    posture: FailurePosture::NotRetryable,
                })?;
        let account_uuid_array: [u8; 16] =
            account_uuid_bytes
                .try_into()
                .map_err(|raw: Vec<u8>| StorageError::SqliteFailed {
                    reason: format!("accounts.uuid had wrong byte length: {}", raw.len()),
                    posture: FailurePosture::NotRetryable,
                })?;
        let account_uuid = uuid::Uuid::from_bytes(account_uuid_array);
        let output_index = u32::try_from(output_index).map_err(|_| StorageError::SqliteFailed {
            reason: format!("received note output_index out of u32 range: {output_index}"),
            posture: FailurePosture::NotRetryable,
        })?;
        let value_zat = u64::try_from(value_zat).map_err(|_| StorageError::SqliteFailed {
            reason: format!("received note value out of u64 range: {value_zat}"),
            posture: FailurePosture::NotRetryable,
        })?;
        let mined_height_u32 =
            u32::try_from(mined_height).map_err(|_| StorageError::SqliteFailed {
                reason: format!("transactions.mined_height out of u32 range: {mined_height}"),
                posture: FailurePosture::NotRetryable,
            })?;
        let protocol = match pool.as_str() {
            "sapling" => zcash_protocol::ShieldedProtocol::Sapling,
            "orchard" => zcash_protocol::ShieldedProtocol::Orchard,
            other => {
                return Err(StorageError::SqliteFailed {
                    reason: format!("unknown pool tag: {other}"),
                    posture: FailurePosture::NotRetryable,
                });
            }
        };
        let block_timestamp_ms = block_timestamp_ms
            .and_then(|raw| u64::try_from(raw).ok())
            .unwrap_or(0);
        rows.push(crate::wallet_storage::ReceivedShieldedNoteRow {
            account_id: AccountId::from_uuid(account_uuid),
            protocol,
            value_zat,
            tx_id: zally_core::TxId::from_bytes(txid_array),
            output_index,
            mined_height: BlockHeight::from(mined_height_u32),
            block_timestamp_ms,
            is_change: is_change != 0,
            spent_our_inputs: spent_our_inputs != 0,
        });
    }
    Ok(rows)
}

struct InMemoryBlockSource {
    blocks: Vec<zcash_client_backend::proto::compact_formats::CompactBlock>,
}

impl zcash_client_backend::data_api::chain::BlockSource for InMemoryBlockSource {
    type Error = std::convert::Infallible;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<zcash_protocol::consensus::BlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> Result<(), zcash_client_backend::data_api::chain::error::Error<WalletErrT, Self::Error>>
    where
        F: FnMut(
            zcash_client_backend::proto::compact_formats::CompactBlock,
        ) -> Result<
            (),
            zcash_client_backend::data_api::chain::error::Error<WalletErrT, Self::Error>,
        >,
    {
        let cap = limit.unwrap_or(usize::MAX);
        let mut taken = 0_usize;
        for block in &self.blocks {
            if let Some(start) = from_height {
                let h = zcash_protocol::consensus::BlockHeight::from_u32(
                    u32::try_from(block.height).unwrap_or(u32::MAX),
                );
                if h < start {
                    continue;
                }
            }
            if taken >= cap {
                break;
            }
            with_block(block.clone())?;
            taken += 1;
        }
        Ok(())
    }
}

/// Maps the upstream `scan_cached_blocks` error into a storage-vocabulary error.
///
/// `ScanError::PrevHashMismatch` (and other continuity errors) mean the chain rolled back
/// between the wallet's last successful scan and this call; the caller must roll the
/// wallet back to before `at_height` and retry. Everything else is opaque sqlite failure.
fn map_scan_error<DbErr, BlockSourceErr>(
    err: &zcash_client_backend::data_api::chain::error::Error<DbErr, BlockSourceErr>,
) -> StorageError
where
    DbErr: std::fmt::Display,
    BlockSourceErr: std::fmt::Display,
{
    use zcash_client_backend::data_api::chain::error::Error as ChainError;
    if let ChainError::Scan(scan) = err
        && scan.is_continuity_error()
    {
        return StorageError::ChainReorgDetected {
            at_height: BlockHeight::from(u32::from(scan.at_height())),
        };
    }
    StorageError::SqliteFailed {
        reason: format!("scan_cached_blocks failed: {err}"),
        posture: FailurePosture::NotRetryable,
    }
}

fn account_uuid_to_zally(uuid: AccountUuid) -> AccountId {
    AccountId::from_uuid(uuid.expose_uuid())
}

fn check_proposal_against_excluded_outpoints<FeeRuleT, N>(
    proposal: &zcash_client_backend::proposal::Proposal<FeeRuleT, N>,
    excluded: &std::collections::HashSet<zally_core::OutPoint>,
    network: Network,
    account_id: AccountId,
) -> Result<(), StorageError> {
    if excluded.is_empty() {
        return Ok(());
    }
    let excluded_count = proposal
        .steps()
        .iter()
        .flat_map(zcash_client_backend::proposal::Step::transparent_inputs)
        .filter(|utxo| {
            let prevout = utxo.outpoint();
            let zally_outpoint = zally_core::OutPoint::new(
                zally_core::TxId::from_bytes(*prevout.hash()),
                prevout.n(),
            );
            excluded.contains(&zally_outpoint)
        })
        .count();
    if excluded_count > 0 {
        tracing::info!(
            target: "zally::storage",
            event = "transparent_inputs_filtered_pending_broadcast",
            network = ?network,
            account_id = %account_id.as_uuid(),
            excluded_count = excluded_count,
            pending_broadcast_count = excluded.len(),
            "transparent inputs locked by pending broadcast were selected by upstream; refusing"
        );
        return Err(StorageError::SqliteFailed {
            reason: format!(
                "insufficient unlocked transparent inputs: {excluded_count} input(s) selected by upstream \
                 are locked by a still-unconfirmed wallet-owned broadcast"
            ),
            posture: FailurePosture::Retryable,
        });
    }
    Ok(())
}

fn clamp_unsigned_to_i64(unsigned: u64) -> i64 {
    i64::try_from(unsigned).unwrap_or(i64::MAX)
}

/// Translates an upstream `zcash_protocol::value::Zatoshis` into Zally's `Zatoshis` newtype.
///
/// Both types carry a `MAX_MONEY`-bounded non-negative zatoshi count, so the conversion is
/// total: `try_from` cannot fail because the upstream value's invariant already enforces
/// the same cap Zally requires.
fn upstream_to_zally_zatoshis(upstream: UpstreamZatoshis) -> Zatoshis {
    Zatoshis::try_from(u64::from(upstream)).unwrap_or_else(|_| Zatoshis::zero())
}

/// Decodes a sqlite signed-integer zatoshi column into a typed `Zatoshis`, failing closed
/// on a negative or above-`MAX_MONEY` value.
fn decode_row_zatoshis(stored: i64, column: &'static str) -> Result<Zatoshis, StorageError> {
    let positive = u64::try_from(stored).map_err(|_| StorageError::RowValueOutOfRange {
        column,
        raw: stored.to_string(),
    })?;
    Zatoshis::try_from(positive).map_err(|_| StorageError::RowValueOutOfRange {
        column,
        raw: positive.to_string(),
    })
}

/// Reads the `ext_zally_observed_tip` row through `ledger` and decodes it into a typed
/// `BlockHeight`. Returns `Ok(None)` when the wallet has never recorded an observed tip.
fn find_observed_tip_on(
    ledger: &rusqlite::Connection,
) -> Result<Option<BlockHeight>, StorageError> {
    let raw = ledger
        .query_row(
            "SELECT tip_height FROM ext_zally_observed_tip WHERE id = 0",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|err| StorageError::SqliteFailed {
            reason: format!("ext_zally_observed_tip lookup failed: {err}"),
            posture: FailurePosture::NotRetryable,
        })?;
    raw.map(|tip_i64| {
        u32::try_from(tip_i64).map(BlockHeight::from).map_err(|_| {
            StorageError::RowValueOutOfRange {
                column: "ext_zally_observed_tip.tip_height",
                raw: tip_i64.to_string(),
            }
        })
    })
    .transpose()
}

fn decode_txid_bytes(bytes: Vec<u8>, label: &'static str) -> Result<TxId, StorageError> {
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|raw: Vec<u8>| StorageError::SqliteFailed {
            reason: format!("{label} had wrong byte length: {}", raw.len()),
            posture: FailurePosture::NotRetryable,
        })?;
    Ok(TxId::from_bytes(array))
}

/// Decodes the big-endian diversifier-index blob stored on the addresses table.
///
/// Mirrors `zcash_client_sqlite::wallet::encoding::decode_diversifier_index_be`, which is
/// crate-private upstream. The blob is 11 bytes, stored big-endian for index ordering;
/// `DiversifierIndex` itself is little-endian internally.
fn decode_diversifier_index_be(
    di_be_bytes: Vec<u8>,
) -> Result<zip32::DiversifierIndex, StorageError> {
    let mut di_be: [u8; 11] =
        di_be_bytes
            .try_into()
            .map_err(|raw: Vec<u8>| StorageError::SqliteFailed {
                reason: format!(
                    "addresses.diversifier_index_be had wrong byte length: {}",
                    raw.len()
                ),
                posture: FailurePosture::NotRetryable,
            })?;
    di_be.reverse();
    Ok(zip32::DiversifierIndex::from(di_be))
}

fn zally_to_account_uuid(id: AccountId) -> AccountUuid {
    AccountUuid::from_uuid(id.as_uuid())
}

fn prepared_transactions_for_txids<'a>(
    db: &Db,
    txids: impl IntoIterator<Item = &'a zcash_protocol::TxId>,
    context: &'static str,
) -> Result<Vec<crate::wallet_storage::PreparedTransaction>, StorageError> {
    let txids_iter = txids.into_iter();
    let (lower_bound, _) = txids_iter.size_hint();
    let mut prepared = Vec::with_capacity(lower_bound);
    for tx_id in txids_iter {
        let stored = zcash_client_backend::data_api::WalletRead::get_transaction(db, *tx_id)
            .map_err(|e| map_sqlite_error(&e))?
            .ok_or_else(|| StorageError::SqliteFailed {
                reason: format!("{context} tx {tx_id} not present in wallet store"),
                posture: FailurePosture::NotRetryable,
            })?;
        let mut raw_bytes = Vec::new();
        stored
            .write(&mut raw_bytes)
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("transaction serialize failed: {err}"),
                posture: FailurePosture::NotRetryable,
            })?;
        prepared.push(crate::wallet_storage::PreparedTransaction::new(
            zally_core::TxId::from_bytes(*tx_id.as_ref()),
            raw_bytes,
        ));
    }
    Ok(prepared)
}

fn transparent_utxo_row_to_output(
    row: crate::wallet_storage::TransparentUtxoRow,
) -> Result<WalletTransparentOutput, StorageError> {
    let output_zat = UpstreamZatoshis::from_u64(row.value_zat).map_err(|_| {
        StorageError::TransparentOutputValueOutOfRange {
            value_zat: row.value_zat,
        }
    })?;
    let outpoint = UpstreamOutPoint::new(*row.tx_id.as_bytes(), row.output_index);
    let txout = TxOut::new(
        output_zat,
        Script(zcash_script::script::Code(row.script_pub_key_bytes)),
    );
    let mined_height = zcash_protocol::consensus::BlockHeight::from(row.mined_height.as_u32());
    WalletTransparentOutput::from_parts(outpoint, txout, Some(mined_height)).ok_or(
        StorageError::TransparentOutputNotRecognized {
            tx_id: row.tx_id,
            output_index: row.output_index,
        },
    )
}

fn map_sqlite_error<E: std::fmt::Display>(err: &E) -> StorageError {
    let reason = err.to_string();
    let lc = reason.to_lowercase();
    if lc.contains("already") || lc.contains("collide") || lc.contains("conflict") {
        return StorageError::AccountAlreadyExists;
    }
    let posture = if lc.contains("locked") || lc.contains("busy") {
        FailurePosture::Retryable
    } else {
        FailurePosture::NotRetryable
    };
    StorageError::SqliteFailed { reason, posture }
}

fn map_derivation_error(err: &KeyDerivationError) -> StorageError {
    StorageError::KeyDerivationFailed {
        reason: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn account_id_translation_is_identity() {
        let uuid = Uuid::new_v4();
        let zally_id = AccountId::from_uuid(uuid);
        let sqlite_uuid = zally_to_account_uuid(zally_id);
        let back = account_uuid_to_zally(sqlite_uuid);
        assert_eq!(zally_id, back);
        assert_eq!(uuid, back.as_uuid());
    }
}
