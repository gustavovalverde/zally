//! `SQLite`-backed wallet storage.
//!
//! Every wallet-db call funnels through a single-threaded **wallet-db actor**.
//! One owned thread owns the [`WalletDb`] and the ledger [`rusqlite::Connection`];
//! callers send type-erased [`DbWork`] closures over a bounded `mpsc` channel and
//! await a `oneshot` reply. This shape replaces the older
//! `Arc<Mutex<Option<Db>>> + spawn_blocking + blocking_lock()` design, which
//! parked an OS thread per call and serialised every operation behind the
//! multi-second proving window held by `prepare_payment` (zk-SNARK proof
//! generation runs while the wallet-db mutex is held). With the actor, that
//! same serialisation still happens (`rusqlite::Connection` is `!Sync` and
//! wants one thread), but the request queue is bounded, observable, and
//! visible to backpressure instead of saturating the tokio blocking pool with
//! 512 parked threads.
//!
//! [`SqliteWalletStorage`] is a cheap [`Clone`] handle holding only the
//! channel sender; the actor lives until every clone is dropped.

use std::path::PathBuf;

use async_trait::async_trait;
use rand::rngs::OsRng;
use rusqlite::OptionalExtension as _;
use secrecy::{ExposeSecret as _, SecretVec};
use tokio::sync::{mpsc, oneshot};
use zally_core::{AccountId, BlockHeight, IdempotencyKey, Network, NetworkParameters, TxId};
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
use zcash_protocol::value::Zatoshis;
use zcash_transparent::address::Script;
use zcash_transparent::bundle::{OutPoint, TxOut};

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

    /// Current number of work items queued on the actor. Exposed for the
    /// runtime's `fauzec_wallet_db_queue_depth` metric: a depth that stays
    /// near `WALLET_DB_QUEUE_CAPACITY` means the actor cannot keep up with
    /// incoming load.
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
            is_retryable: false,
        })?;
    }
    let params = options.network.to_parameters();
    let mut db = WalletDb::for_path(&options.db_path, params, SystemClock, OsRng).map_err(|e| {
        StorageError::SqliteFailed {
            reason: e.to_string(),
            is_retryable: false,
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
            is_retryable: false,
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
             );",
        )
        .map_err(|e| StorageError::SqliteFailed {
            reason: format!("ext_zally schema init failed: {e}"),
            is_retryable: false,
        })?;

    Ok(WalletDbState::Opened {
        db: Box::new(db),
        ledger: Box::new(ledger),
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "async-trait fans the WalletStorage methods into a single expanded impl; \
              extracting helpers per method would obscure the trait-method boundary"
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

        // `AllAvailableKeys` resolves the request against the actual UFVK: P2PKH becomes
        // `Require` because Zally's UFVKs carry a transparent component, which routes the
        // upstream into the gap-limit pre-generation path. The first call to this method
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
                    is_retryable: false,
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
                    is_retryable: false,
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
        let amount =
            zcash_protocol::value::Zatoshis::from_u64(request.amount_zat).map_err(|err| {
                StorageError::SqliteFailed {
                    reason: format!(
                        "amount {} exceeds Zatoshis maximum: {err}",
                        request.amount_zat
                    ),
                    is_retryable: false,
                }
            })?;
        let recipient = zcash_keys::address::Address::decode(&params, &request.recipient_encoded)
            .ok_or_else(|| StorageError::SqliteFailed {
            reason: format!("could not decode recipient '{}'", request.recipient_encoded),
            is_retryable: false,
        })?;
        let memo_bytes = request
            .memo
            .as_deref()
            .map(zcash_protocol::memo::MemoBytes::from_bytes)
            .transpose()
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("memo too long: {err}"),
                is_retryable: false,
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
                    is_retryable: false,
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
        let amount =
            zcash_protocol::value::Zatoshis::from_u64(request.amount_zat).map_err(|err| {
                StorageError::SqliteFailed {
                    reason: format!(
                        "amount {} exceeds Zatoshis maximum: {err}",
                        request.amount_zat
                    ),
                    is_retryable: false,
                }
            })?;
        let recipient = zcash_keys::address::Address::decode(&params, &request.recipient_encoded)
            .ok_or_else(|| StorageError::SqliteFailed {
            reason: format!("could not decode recipient '{}'", request.recipient_encoded),
            is_retryable: false,
        })?;
        let memo_bytes = request
            .memo
            .as_deref()
            .map(zcash_protocol::memo::MemoBytes::from_bytes)
            .transpose()
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("memo too long: {err}"),
                is_retryable: false,
            })?;
        let prover = zcash_proofs::prover::LocalTxProver::with_default_location()
            .ok_or(StorageError::ProverUnavailable)?;
        let seed_bytes = SecretVec::new(seed.expose_secret().to_vec());

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
                    is_retryable: false,
                })?;

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
                is_retryable: false,
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
        let shielding_threshold =
            Zatoshis::from_u64(request.shielding_threshold_zat).map_err(|_| {
                StorageError::TransparentOutputValueOutOfRange {
                    value_zat: request.shielding_threshold_zat,
                }
            })?;
        let prover = zcash_proofs::prover::LocalTxProver::with_default_location()
            .ok_or(StorageError::ProverUnavailable)?;
        let seed_bytes = SecretVec::new(seed.expose_secret().to_vec());

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
            let txids = zcash_client_backend::data_api::wallet::shield_transparent_funds(
                db,
                &params,
                &prover,
                &prover,
                &input_selector,
                &change_strategy,
                shielding_threshold,
                &spending_keys,
                &from_addrs,
                account_uuid,
                ConfirmationsPolicy::default(),
            )
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("shield_transparent_funds failed: {err}"),
                is_retryable: false,
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
        let amount =
            zcash_protocol::value::Zatoshis::from_u64(request.amount_zat).map_err(|err| {
                StorageError::SqliteFailed {
                    reason: format!(
                        "amount {} exceeds Zatoshis maximum: {err}",
                        request.amount_zat
                    ),
                    is_retryable: false,
                }
            })?;
        let recipient = zcash_keys::address::Address::decode(&params, &request.recipient_encoded)
            .ok_or_else(|| StorageError::SqliteFailed {
            reason: format!("could not decode recipient '{}'", request.recipient_encoded),
            is_retryable: false,
        })?;
        let memo_bytes = request
            .memo
            .as_deref()
            .map(zcash_protocol::memo::MemoBytes::from_bytes)
            .transpose()
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("memo too long: {err}"),
                is_retryable: false,
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
                    is_retryable: false,
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
                is_retryable: false,
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
            is_retryable: false,
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
                    is_retryable: false,
                })?;

            let stored = zcash_client_backend::data_api::WalletRead::get_transaction(db, tx_id)
                .map_err(|e| map_sqlite_error(&e))?
                .ok_or_else(|| StorageError::SqliteFailed {
                    reason: format!("extracted tx {tx_id} not present in wallet store"),
                    is_retryable: false,
                })?;
            let mut raw_bytes = Vec::new();
            stored
                .write(&mut raw_bytes)
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("transaction serialize failed: {err}"),
                    is_retryable: false,
                })?;
            Ok(crate::wallet_storage::PreparedTransaction::new(
                zally_core::TxId::from_bytes(*tx_id.as_ref()),
                raw_bytes,
            ))
        })
        .await
    }

    async fn lookup_idempotent_submission(
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
                    is_retryable: false,
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
                                is_retryable: false,
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
                    is_retryable: false,
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
                is_retryable: false,
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
                    is_retryable: false,
                })?;
            let rows = stmt
                .query_map([start_h, end_h], |row| {
                    let txid_bytes: Vec<u8> = row.get(0)?;
                    let mined_height: i64 = row.get(1)?;
                    Ok((txid_bytes, mined_height))
                })
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("transactions range query failed: {err}"),
                    is_retryable: false,
                })?;
            let mut entries = Vec::new();
            for row in rows {
                let (txid_bytes, mined_height) = row.map_err(|err| StorageError::SqliteFailed {
                    reason: format!("transactions row decode failed: {err}"),
                    is_retryable: false,
                })?;
                let array: [u8; 32] =
                    txid_bytes
                        .try_into()
                        .map_err(|raw: Vec<u8>| StorageError::SqliteFailed {
                            reason: format!(
                                "transactions.txid had wrong byte length: {}",
                                raw.len()
                            ),
                            is_retryable: false,
                        })?;
                let height =
                    u32::try_from(mined_height).map_err(|_| StorageError::SqliteFailed {
                        reason: format!(
                            "transactions.mined_height out of u32 range: {mined_height}"
                        ),
                        is_retryable: false,
                    })?;
                entries.push((TxId::from_bytes(array), BlockHeight::from(height)));
            }
            Ok(entries)
        })
        .await
    }

    async fn lookup_observed_tip(&self) -> Result<Option<BlockHeight>, StorageError> {
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
                    is_retryable: false,
                })?;
            outcome
                .map(|raw| {
                    u32::try_from(raw).map(BlockHeight::from).map_err(|_| {
                        StorageError::SqliteFailed {
                            reason: format!("ext_zally_observed_tip.tip_height out of u32: {raw}"),
                            is_retryable: false,
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
                is_retryable: false,
            })?;
            Ok(())
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
                is_retryable: false,
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
                    is_retryable: false,
                })?;
            let mapped = stmt
                .query_map([from_h, to_h], decode_received_shielded_note_row)
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("received_shielded_notes query failed: {err}"),
                    is_retryable: false,
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
                    is_retryable: false,
                })?;
            let mapped = stmt
                .query_map([&account_uuid_bytes], decode_received_shielded_note_row)
                .map_err(|err| StorageError::SqliteFailed {
                    reason: format!("list_shielded_receives query failed: {err}"),
                    is_retryable: false,
                })?;
            collect_received_shielded_note_rows(mapped)
        })
        .await
    }
}

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
            is_retryable: false,
        })?;
        let txid_array: [u8; 32] =
            txid_bytes
                .try_into()
                .map_err(|raw: Vec<u8>| StorageError::SqliteFailed {
                    reason: format!("transactions.txid had wrong byte length: {}", raw.len()),
                    is_retryable: false,
                })?;
        let account_uuid_array: [u8; 16] =
            account_uuid_bytes
                .try_into()
                .map_err(|raw: Vec<u8>| StorageError::SqliteFailed {
                    reason: format!("accounts.uuid had wrong byte length: {}", raw.len()),
                    is_retryable: false,
                })?;
        let account_uuid = uuid::Uuid::from_bytes(account_uuid_array);
        let output_index = u32::try_from(output_index).map_err(|_| StorageError::SqliteFailed {
            reason: format!("received note output_index out of u32 range: {output_index}"),
            is_retryable: false,
        })?;
        let value_zat = u64::try_from(value_zat).map_err(|_| StorageError::SqliteFailed {
            reason: format!("received note value out of u64 range: {value_zat}"),
            is_retryable: false,
        })?;
        let mined_height_u32 =
            u32::try_from(mined_height).map_err(|_| StorageError::SqliteFailed {
                reason: format!("transactions.mined_height out of u32 range: {mined_height}"),
                is_retryable: false,
            })?;
        let protocol = match pool.as_str() {
            "sapling" => zcash_protocol::ShieldedProtocol::Sapling,
            "orchard" => zcash_protocol::ShieldedProtocol::Orchard,
            other => {
                return Err(StorageError::SqliteFailed {
                    reason: format!("unknown pool tag: {other}"),
                    is_retryable: false,
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
        is_retryable: false,
    }
}

fn account_uuid_to_zally(uuid: AccountUuid) -> AccountId {
    AccountId::from_uuid(uuid.expose_uuid())
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
                is_retryable: false,
            })?;
        let mut raw_bytes = Vec::new();
        stored
            .write(&mut raw_bytes)
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("transaction serialize failed: {err}"),
                is_retryable: false,
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
    let output_zat = Zatoshis::from_u64(row.value_zat).map_err(|_| {
        StorageError::TransparentOutputValueOutOfRange {
            value_zat: row.value_zat,
        }
    })?;
    let outpoint = OutPoint::new(*row.tx_id.as_bytes(), row.output_index);
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
    let is_retryable = lc.contains("locked") || lc.contains("busy");
    StorageError::SqliteFailed {
        reason,
        is_retryable,
    }
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
