//! `SQLite`-backed wallet storage.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rand::rngs::OsRng;
use rusqlite::OptionalExtension as _;
use secrecy::{ExposeSecret as _, SecretVec};
use tokio::sync::Mutex;
use zally_core::{AccountId, BlockHeight, IdempotencyKey, Network, NetworkParameters, TxId};
use zally_keys::{KeyDerivationError, SeedMaterial, derive_ufvk};
use zcash_client_backend::data_api::chain::ChainState;
use zcash_client_backend::data_api::{Account, AccountBirthday, WalletRead, WalletWrite};
use zcash_client_sqlite::AccountUuid;
use zcash_client_sqlite::WalletDb;
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::wallet::init::WalletMigrator;
use zcash_keys::address::UnifiedAddress;
use zcash_keys::keys::UnifiedAddressRequest;
use zcash_primitives::block::BlockHash;

use crate::storage_error::StorageError;
use crate::wallet_storage::WalletStorage;

type Db = WalletDb<rusqlite::Connection, NetworkParameters, SystemClock, OsRng>;

const DEFAULT_ACCOUNT_NAME: &str = "primary";

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
    /// Production options. The account name defaults to `"primary"`.
    #[must_use]
    pub fn for_network(network: Network, db_path: PathBuf) -> Self {
        Self {
            db_path,
            network,
            account_name: DEFAULT_ACCOUNT_NAME.to_owned(),
        }
    }

    /// Options for local tests. The account name defaults to `"primary"`.
    #[must_use]
    pub fn for_local_tests(db_path: PathBuf) -> Self {
        Self {
            db_path,
            network: Network::regtest_all_at_genesis(),
            account_name: DEFAULT_ACCOUNT_NAME.to_owned(),
        }
    }
}

/// `SQLite`-backed [`WalletStorage`] implementation.
///
/// Wraps [`zcash_client_sqlite::WalletDb`]. The wallet database is opened lazily; until
/// [`SqliteWalletStorage::open_or_create`] runs, the inner `Option<Db>` is `None`. Every
/// public method routes blocking sqlite work through [`tokio::task::spawn_blocking`] via
/// per-call internal helpers (`with_db` / `with_db_mut`); later slices add methods by
/// composing through the same shape.
pub struct SqliteWalletStorage {
    options: SqliteWalletStorageOptions,
    inner: Arc<Mutex<Option<Db>>>,
    ledger: Arc<Mutex<Option<rusqlite::Connection>>>,
}

impl SqliteWalletStorage {
    /// Constructs a new storage handle. The database is not opened until
    /// [`SqliteWalletStorage::open_or_create`] is called.
    #[must_use]
    pub fn new(options: SqliteWalletStorageOptions) -> Self {
        Self {
            options,
            inner: Arc::new(Mutex::new(None)),
            ledger: Arc::new(Mutex::new(None)),
        }
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard must live for the duration of `work` because `conn` borrows from it"
    )]
    async fn with_ledger<F, T>(&self, work: F) -> Result<T, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, StorageError> + Send + 'static,
        T: Send + 'static,
    {
        let ledger = Arc::clone(&self.ledger);
        tokio::task::spawn_blocking(move || {
            let guard = ledger.blocking_lock();
            let conn = guard.as_ref().ok_or(StorageError::NotOpened)?;
            let outcome = work(conn);
            drop(guard);
            outcome
        })
        .await
        .map_err(|e| StorageError::BlockingTaskFailed {
            reason: e.to_string(),
        })?
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard must live for the duration of `work` because `db` borrows from it"
    )]
    async fn with_db<F, T>(&self, work: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Db) -> Result<T, StorageError> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_lock();
            let db = guard.as_ref().ok_or(StorageError::NotOpened)?;
            let outcome = work(db);
            drop(guard);
            outcome
        })
        .await
        .map_err(|e| StorageError::BlockingTaskFailed {
            reason: e.to_string(),
        })?
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard must live for the duration of `work` because `db` borrows from it"
    )]
    async fn with_db_mut<F, T>(&self, work: F) -> Result<T, StorageError>
    where
        F: FnOnce(&mut Db) -> Result<T, StorageError> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            let db = guard.as_mut().ok_or(StorageError::NotOpened)?;
            let outcome = work(db);
            drop(guard);
            outcome
        })
        .await
        .map_err(|e| StorageError::BlockingTaskFailed {
            reason: e.to_string(),
        })?
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "async-trait fans the WalletStorage methods into a single expanded impl; \
              extracting helpers per method would obscure the trait-method boundary"
)]
#[async_trait]
impl WalletStorage for SqliteWalletStorage {
    async fn open_or_create(&self) -> Result<(), StorageError> {
        let inner = Arc::clone(&self.inner);
        let ledger = Arc::clone(&self.ledger);
        let db_path = self.options.db_path.clone();
        let params = self.options.network.to_parameters();

        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            let mut db_guard = inner.blocking_lock();
            let mut ledger_guard = ledger.blocking_lock();
            if db_guard.is_some() && ledger_guard.is_some() {
                return Ok(());
            }
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| StorageError::SqliteFailed {
                    reason: format!("could not create database directory: {e}"),
                    is_retryable: false,
                })?;
            }
            let mut db = WalletDb::for_path(&db_path, params, SystemClock, OsRng).map_err(|e| {
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
            *db_guard = Some(db);

            let conn =
                rusqlite::Connection::open(&db_path).map_err(|e| StorageError::SqliteFailed {
                    reason: format!("ledger connection open failed: {e}"),
                    is_retryable: false,
                })?;
            conn.execute_batch(
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
            *ledger_guard = Some(conn);

            drop(db_guard);
            drop(ledger_guard);
            Ok(())
        })
        .await
        .map_err(|e| StorageError::BlockingTaskFailed {
            reason: e.to_string(),
        })?
    }

    async fn create_account_for_seed(
        &self,
        seed: &SeedMaterial,
        birthday: BlockHeight,
    ) -> Result<AccountId, StorageError> {
        let account_name = self.options.account_name.clone();
        let seed_bytes = seed.expose_secret().to_vec();
        let secret = SecretVec::new(seed_bytes);

        self.with_db_mut(move |db| {
            let birthday_height: zcash_protocol::consensus::BlockHeight = birthday.into();
            let account_birthday = build_birthday(birthday);
            let (account, _usk) = db
                .import_account_hd(
                    &account_name,
                    &secret,
                    zip32::AccountId::ZERO,
                    &account_birthday,
                    None,
                )
                .map_err(|e| map_sqlite_error(&e))?;
            // Seed the wallet's view of the chain tip at the birthday height. Without this,
            // `get_next_available_address` rejects with "Chain height unknown". Slice 2's
            // sync loop advances the tip as blocks are scanned.
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
            .map_err(|err| StorageError::SqliteFailed {
                reason: format!("scan_cached_blocks failed: {err}"),
                is_retryable: false,
            })
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

    async fn fully_scanned_height(&self) -> Result<Option<BlockHeight>, StorageError> {
        self.with_db(move |db| {
            let summary = db
                .get_wallet_summary(
                    zcash_client_backend::data_api::wallet::ConfirmationsPolicy::MIN,
                )
                .map_err(|e| map_sqlite_error(&e))?;
            Ok(summary.map(|s| BlockHeight::from(u32::from(s.fully_scanned_height()))))
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
                    zcash_client_backend::data_api::wallet::ConfirmationsPolicy::MIN,
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
            let spending_keys =
                zcash_client_backend::data_api::wallet::SpendingKeys::from_unified_spending_key(
                    usk,
                );

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
                    zcash_client_backend::data_api::wallet::ConfirmationsPolicy::MIN,
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

            let mut prepared = Vec::with_capacity(txids.len());
            for tx_id in &txids {
                let stored =
                    zcash_client_backend::data_api::WalletRead::get_transaction(db, *tx_id)
                        .map_err(|e| map_sqlite_error(&e))?
                        .ok_or_else(|| StorageError::SqliteFailed {
                            reason: format!("created tx {tx_id} not present in wallet store"),
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
                    zcash_client_backend::data_api::wallet::ConfirmationsPolicy::MIN,
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
                 ON CONFLICT(id) DO UPDATE SET tip_height = excluded.tip_height \
                     WHERE excluded.tip_height > ext_zally_observed_tip.tip_height",
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

fn account_uuid_to_zally(uuid: AccountUuid) -> AccountId {
    AccountId::from_uuid(uuid.expose_uuid())
}

fn zally_to_account_uuid(id: AccountId) -> AccountUuid {
    AccountUuid::from_uuid(id.as_uuid())
}

fn build_birthday(height: BlockHeight) -> AccountBirthday {
    let prior_height: zcash_protocol::consensus::BlockHeight = height.saturating_sub(1).into();
    let chain_state = ChainState::empty(prior_height, BlockHash([0u8; 32]));
    AccountBirthday::from_parts(chain_state, None)
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
