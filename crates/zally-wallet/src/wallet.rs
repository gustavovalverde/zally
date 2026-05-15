//! Operator-facing wallet handle.

use std::collections::BTreeSet;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::broadcast;
use zally_chain::{ChainSource, ChainSourceError};
use zally_core::{AccountId, BlockHeight, Network};
use zally_keys::{Mnemonic, SealingError, SeedMaterial, SeedSealing};
use zally_storage::{StorageError, WalletStorage};
use zcash_client_backend::data_api::chain::ChainState;
use zcash_keys::address::UnifiedAddress;

use crate::capabilities::{Capability, SealingCapability, StorageCapability, WalletCapabilities};
use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerState};
use crate::event::{WalletEvent, WalletEventStream};
use crate::retry::RetryPolicy;
use crate::wallet_error::WalletError;

const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Operator-facing wallet handle.
///
/// Cheap to clone; cloning shares the inner sealing and storage handles via `Arc`. All async
/// methods are cancellation-safe. Zally holds one account per wallet; the `AccountId`
/// returned by [`Wallet::create`] and [`Wallet::open`] names that single account.
#[derive(Clone)]
pub struct Wallet {
    pub(crate) inner: Arc<WalletInner>,
}

pub(crate) struct WalletInner {
    pub(crate) network: Network,
    pub(crate) sealing: Box<dyn SeedSealing>,
    pub(crate) storage: Box<dyn WalletStorage>,
    pub(crate) base_capabilities: WalletCapabilities,
    pub(crate) event_tx: broadcast::Sender<WalletEvent>,
    pub(crate) retry_policy: Mutex<RetryPolicy>,
    pub(crate) circuit_breaker: CircuitBreaker,
}

impl Wallet {
    /// Creates a new wallet.
    ///
    /// Generates a 24-word BIP-39 mnemonic, derives the seed, seals it via the provided
    /// sealing implementation, opens (or creates) the storage, and creates the wallet's first
    /// account at `birthday`.
    ///
    /// Returns the wallet handle, the new account's `AccountId`, and the generated `Mnemonic`.
    /// The operator must record the mnemonic out-of-band; Zally does not back it up.
    ///
    /// Returns [`WalletError::AccountAlreadyExists`] if the storage already has an account.
    /// Returns [`WalletError::NetworkMismatch`] if `network != storage.network()`.
    ///
    /// `requires_operator` on `AccountAlreadyExists`. `retryable` on transient I/O.
    pub async fn create<S, St>(
        chain: &dyn ChainSource,
        network: Network,
        sealing: S,
        storage: St,
        birthday: BlockHeight,
    ) -> Result<(Self, AccountId, Mnemonic), WalletError>
    where
        S: SeedSealing,
        St: WalletStorage,
    {
        let sealing_capability = capability_for_sealing::<S>();
        let storage_capability = capability_for_storage::<St>();

        if chain.network() != network {
            return Err(WalletError::NetworkMismatch {
                storage: chain.network(),
                requested: network,
            });
        }
        if storage.network() != network {
            return Err(WalletError::NetworkMismatch {
                storage: storage.network(),
                requested: network,
            });
        }

        storage.open_or_create().await?;

        match sealing.unseal_seed().await {
            Ok(existing_seed) => {
                let existing = storage.find_account_for_seed(&existing_seed).await?;
                if existing.is_some() {
                    return Err(WalletError::AccountAlreadyExists);
                }
                return Err(WalletError::AccountAlreadyExists);
            }
            Err(SealingError::NoSealedSeed) => {}
            Err(e) => return Err(WalletError::from(e)),
        }

        let mnemonic = Mnemonic::generate();
        let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
        sealing.seal_seed(&seed).await?;
        let prior_chain_state = fetch_prior_chain_state(chain, birthday).await?;
        let account_id = storage
            .create_account_for_seed(&seed, prior_chain_state)
            .await?;

        let capabilities = build_capabilities(network, sealing_capability, storage_capability);
        emit_plaintext_warning_if_needed(&capabilities, "create");

        let (event_tx, _rx_keepalive) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let inner = Arc::new(WalletInner {
            network,
            sealing: Box::new(sealing),
            storage: Box::new(storage),
            base_capabilities: capabilities,
            event_tx,
            retry_policy: Mutex::new(RetryPolicy::default()),
            circuit_breaker: CircuitBreaker::new(CircuitBreakerConfig::default()),
        });
        Ok((Self { inner }, account_id, mnemonic))
    }

    /// Opens an existing wallet.
    ///
    /// Unseals the existing seed, opens (idempotently) the storage, and looks up the account
    /// whose UFVK matches the seed.
    ///
    /// Returns [`WalletError::NoSealedSeed`] if no sealed seed exists; switch to
    /// [`Wallet::create`]. Returns [`WalletError::AccountNotFound`] if no account in storage
    /// matches the unsealed seed. Returns [`WalletError::NetworkMismatch`] if
    /// `network != storage.network()`.
    ///
    /// `requires_operator` on `NoSealedSeed` or `AccountNotFound`. `retryable` on transient I/O.
    pub async fn open<S, St>(
        network: Network,
        sealing: S,
        storage: St,
    ) -> Result<(Self, AccountId), WalletError>
    where
        S: SeedSealing,
        St: WalletStorage,
    {
        let sealing_capability = capability_for_sealing::<S>();
        let storage_capability = capability_for_storage::<St>();

        if storage.network() != network {
            return Err(WalletError::NetworkMismatch {
                storage: storage.network(),
                requested: network,
            });
        }

        storage.open_or_create().await?;

        let seed = match sealing.unseal_seed().await {
            Ok(s) => s,
            Err(SealingError::NoSealedSeed) => return Err(WalletError::NoSealedSeed),
            Err(e) => return Err(WalletError::from(e)),
        };
        let account_id = storage
            .find_account_for_seed(&seed)
            .await?
            .ok_or(WalletError::AccountNotFound)?;

        let capabilities = build_capabilities(network, sealing_capability, storage_capability);
        emit_plaintext_warning_if_needed(&capabilities, "open");

        let (event_tx, _rx_keepalive) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let inner = Arc::new(WalletInner {
            network,
            sealing: Box::new(sealing),
            storage: Box::new(storage),
            base_capabilities: capabilities,
            event_tx,
            retry_policy: Mutex::new(RetryPolicy::default()),
            circuit_breaker: CircuitBreaker::new(CircuitBreakerConfig::default()),
        });
        Ok((Self { inner }, account_id))
    }

    /// Opens an existing wallet, or creates its single account from the sealed seed.
    ///
    /// Behaves like [`Wallet::open`] when storage already has an account that matches the
    /// unsealed seed. On a fresh persistent volume (storage is initialized but the account row
    /// is missing), the call creates the account at `birthday` from the unsealed seed, then
    /// returns the same handle. Idempotent across boots: once the account exists, `birthday` is
    /// ignored.
    ///
    /// The intended caller is a deployment whose sealed seed is provisioned through a
    /// secret store and whose persistent volume can be re-created from scratch. Operators
    /// who run the wallet on the same machine that ran `Wallet::create` should keep using
    /// [`Wallet::open`].
    ///
    /// Returns [`WalletError::NoSealedSeed`] if no sealed seed exists. Returns
    /// [`WalletError::NetworkMismatch`] if `network != storage.network()`.
    ///
    /// `requires_operator` on `NoSealedSeed`. `retryable` on transient I/O.
    pub async fn open_or_create_account<S, St>(
        chain: &dyn ChainSource,
        network: Network,
        sealing: S,
        storage: St,
        birthday: BlockHeight,
    ) -> Result<(Self, AccountId), WalletError>
    where
        S: SeedSealing,
        St: WalletStorage,
    {
        let sealing_capability = capability_for_sealing::<S>();
        let storage_capability = capability_for_storage::<St>();

        if chain.network() != network {
            return Err(WalletError::NetworkMismatch {
                storage: chain.network(),
                requested: network,
            });
        }
        if storage.network() != network {
            return Err(WalletError::NetworkMismatch {
                storage: storage.network(),
                requested: network,
            });
        }

        storage.open_or_create().await?;

        let seed = match sealing.unseal_seed().await {
            Ok(s) => s,
            Err(SealingError::NoSealedSeed) => return Err(WalletError::NoSealedSeed),
            Err(e) => return Err(WalletError::from(e)),
        };

        let account_id = if let Some(existing) = storage.find_account_for_seed(&seed).await? {
            existing
        } else {
            let prior_chain_state = fetch_prior_chain_state(chain, birthday).await?;
            storage
                .create_account_for_seed(&seed, prior_chain_state)
                .await?
        };

        let capabilities = build_capabilities(network, sealing_capability, storage_capability);
        emit_plaintext_warning_if_needed(&capabilities, "open_or_create_account");

        let (event_tx, _rx_keepalive) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let inner = Arc::new(WalletInner {
            network,
            sealing: Box::new(sealing),
            storage: Box::new(storage),
            base_capabilities: capabilities,
            event_tx,
            retry_policy: Mutex::new(RetryPolicy::default()),
            circuit_breaker: CircuitBreaker::new(CircuitBreakerConfig::default()),
        });
        Ok((Self { inner }, account_id))
    }

    /// Returns the network this wallet is bound to.
    #[must_use]
    pub fn network(&self) -> Network {
        self.inner.network
    }

    /// Returns the runtime capability descriptor. When the wallet's [`CircuitBreaker`] is
    /// open, the returned `features` set includes [`Capability::CircuitBroken`].
    #[must_use]
    pub fn capabilities(&self) -> WalletCapabilities {
        let mut snapshot = self.inner.base_capabilities.clone();
        if matches!(
            self.inner.circuit_breaker.state(),
            CircuitBreakerState::Open
        ) {
            snapshot.features.insert(Capability::CircuitBroken);
        }
        snapshot
    }

    /// Returns the current circuit breaker state.
    #[must_use]
    pub fn circuit_breaker_state(&self) -> CircuitBreakerState {
        self.inner.circuit_breaker.state()
    }

    /// Returns the current [`RetryPolicy`] that governs outbound IO.
    #[must_use]
    pub fn retry_policy(&self) -> RetryPolicy {
        *self.inner.retry_policy.lock()
    }

    /// Replaces the wallet's [`RetryPolicy`]. Subsequent outbound calls (chain reads,
    /// submitter calls, sealed-seed reads, storage IO) use the new policy. Existing calls
    /// in flight finish under the prior policy.
    pub fn set_retry_policy(&self, policy: RetryPolicy) {
        *self.inner.retry_policy.lock() = policy;
    }

    /// Derives, persists, and marks-as-exposed the next available Unified Address for
    /// `account_id` with Sapling + Orchard receivers (no transparent). Each call walks
    /// forward through diversifier indices per ZIP-316. Free of the BIP-44 transparent
    /// gap-limit; suitable as the default receive-address allocator.
    ///
    /// `not_retryable` on unknown account. `retryable` on transient I/O.
    pub async fn derive_next_address(
        &self,
        account_id: AccountId,
    ) -> Result<UnifiedAddress, WalletError> {
        self.inner
            .storage
            .derive_next_address(account_id)
            .await
            .map_err(lift_storage_to_wallet_error)
    }

    /// Derives a Unified Address that also carries a P2PKH (transparent) receiver.
    ///
    /// Subject to the upstream BIP-44 transparent gap limit (10 unused addresses by
    /// default): on a fresh wallet only one call succeeds before an on-chain transaction
    /// must credit a reserved transparent address. Use [`Wallet::derive_next_address`] for
    /// the unbounded shielded-only stream.
    ///
    /// `not_retryable` on gap-limit exhaustion or unknown account; `retryable` on transient I/O.
    pub async fn derive_next_address_with_transparent(
        &self,
        account_id: AccountId,
    ) -> Result<UnifiedAddress, WalletError> {
        self.inner
            .storage
            .derive_next_address_with_transparent(account_id)
            .await
            .map_err(lift_storage_to_wallet_error)
    }

    /// Returns every unspent Sapling and Orchard note owned by `account_id`.
    ///
    /// The wallet uses its persisted observed chain tip (the highest tip reported to it by
    /// [`Wallet::sync`]) to compute the `confirmations` field. Operators that need a fresh
    /// confirmation count should call [`Wallet::sync`] before this method.
    ///
    /// When the wallet has not yet observed a tip (no prior `sync`), `confirmations` is set
    /// to `0` and `mined_height` carries the note's actual mined height.
    ///
    /// `not_retryable` on unknown account; `retryable` on transient storage I/O.
    pub async fn list_unspent_shielded_notes(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<crate::unspent_note::UnspentShieldedNote>, WalletError> {
        let observed_tip = self
            .inner
            .storage
            .lookup_observed_tip()
            .await
            .map_err(lift_storage_to_wallet_error)?;
        let target = observed_tip.unwrap_or_else(|| BlockHeight::from(0));
        let rows = self
            .inner
            .storage
            .list_unspent_shielded_notes(account_id, target)
            .await
            .map_err(lift_storage_to_wallet_error)?;
        Ok(rows
            .into_iter()
            .map(|row| translate_unspent_note(row, observed_tip))
            .collect())
    }

    /// Returns every Sapling and Orchard note ever received by `account_id`, spent or
    /// unspent. Each record carries the provenance fields (`is_change`, `spent_our_inputs`)
    /// that let a downstream observer classify the receive identically to the matching
    /// [`WalletEvent::ShieldedReceiveObserved`] from the live event stream.
    ///
    /// Powers operator-side replays that rebuild downstream observation tables from chain
    /// truth without coupling to the wallet's event stream. Idempotent on the upstream
    /// side: callers should deduplicate by `(tx_id, output_index, pool)`.
    ///
    /// `not_retryable` on unknown account; `retryable` on transient storage I/O.
    pub async fn list_shielded_receives(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<crate::received_note::ShieldedReceiveRecord>, WalletError> {
        let rows = self
            .inner
            .storage
            .list_shielded_receives_for_account(account_id)
            .await
            .map_err(lift_storage_to_wallet_error)?;
        Ok(rows.into_iter().map(translate_received_note).collect())
    }

    /// Subscribes to wallet events. The returned stream stays valid until either the wallet
    /// is dropped or the consumer drops the stream.
    #[must_use]
    pub fn observe(&self) -> WalletEventStream {
        WalletEventStream::from_broadcast(self.inner.event_tx.subscribe())
    }

    /// Publishes an event. Best-effort: silently no-ops if no observers are attached.
    pub(crate) fn publish_event(&self, event: WalletEvent) {
        let _ = self.inner.event_tx.send(event);
    }

    /// Returns the number of subscribers currently attached to [`Wallet::observe`].
    pub(crate) fn observer_count(&self) -> u32 {
        u32::try_from(self.inner.event_tx.receiver_count()).unwrap_or(u32::MAX)
    }
}

async fn fetch_prior_chain_state(
    chain: &dyn ChainSource,
    birthday: BlockHeight,
) -> Result<ChainState, WalletError> {
    let prior_height = BlockHeight::from(birthday.as_u32().saturating_sub(1));
    let tree_state = chain
        .tree_state_at(prior_height)
        .await
        .map_err(|err| map_chain_source_to_wallet_error(&err))?;
    tree_state
        .to_chain_state()
        .map_err(|io| WalletError::ChainSource {
            reason: format!(
                "invalid tree state for birthday {}: {io}",
                birthday.as_u32()
            ),
            is_retryable: false,
        })
}

fn map_chain_source_to_wallet_error(err: &ChainSourceError) -> WalletError {
    WalletError::ChainSource {
        reason: err.to_string(),
        is_retryable: err.is_retryable(),
    }
}

fn lift_storage_to_wallet_error(err: StorageError) -> WalletError {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "StorageError is #[non_exhaustive]; the explicit arm names AccountNotFound \
                  (lifted to WalletError::AccountNotFound), the rest delegates to the From impl"
    )]
    match err {
        StorageError::AccountNotFound => WalletError::AccountNotFound,
        other => WalletError::Storage(other),
    }
}

fn build_capabilities(
    network: Network,
    sealing: SealingCapability,
    storage: StorageCapability,
) -> WalletCapabilities {
    let mut features = BTreeSet::new();
    features.insert(Capability::Zip316UnifiedAddresses);
    features.insert(Capability::Zip302Memos);
    features.insert(Capability::Zip320TexAddresses);
    features.insert(Capability::Zip317ConventionalFee);
    features.insert(Capability::SyncIncremental);
    features.insert(Capability::SyncDriver);
    features.insert(Capability::EventStream);
    features.insert(Capability::IdempotentSend);
    features.insert(Capability::PcztV06);
    features.insert(Capability::MetricsSnapshot);
    features.insert(Capability::StatusSnapshot);
    WalletCapabilities {
        network,
        sealing,
        storage,
        features,
    }
}

fn emit_plaintext_warning_if_needed(capabilities: &WalletCapabilities, event_suffix: &str) {
    #[cfg(feature = "unsafe_plaintext_seed")]
    {
        if capabilities.sealing == SealingCapability::Plaintext {
            tracing::warn!(
                target: "zally::wallet",
                event = "plaintext_seed_in_use",
                phase = event_suffix,
                "wallet opened with plaintext seed sealing; never use in production"
            );
        }
    }
    let _ = (capabilities, event_suffix);
}

fn capability_for_sealing<S: SeedSealing>() -> SealingCapability {
    let name = std::any::type_name::<S>();
    if name.ends_with("AgeFileSealing") || name.contains("::AgeFileSealing<") {
        SealingCapability::AgeFile
    } else if name.ends_with("InMemorySealing") || name.contains("::InMemorySealing<") {
        SealingCapability::InMemory
    } else {
        #[cfg(feature = "unsafe_plaintext_seed")]
        {
            if name.ends_with("PlaintextSealing") || name.contains("::PlaintextSealing<") {
                return SealingCapability::Plaintext;
            }
        }
        SealingCapability::Custom
    }
}

fn capability_for_storage<St: WalletStorage>() -> StorageCapability {
    let name = std::any::type_name::<St>();
    if name.ends_with("SqliteWalletStorage") || name.contains("::SqliteWalletStorage<") {
        StorageCapability::Sqlite
    } else {
        StorageCapability::Custom
    }
}

fn translate_received_note(
    row: zally_storage::ReceivedShieldedNoteRow,
) -> crate::received_note::ShieldedReceiveRecord {
    let pool = match row.protocol {
        zcash_protocol::ShieldedProtocol::Sapling => zally_chain::ShieldedPool::Sapling,
        zcash_protocol::ShieldedProtocol::Orchard => zally_chain::ShieldedPool::Orchard,
    };
    let amount = zally_core::Zatoshis::try_from(row.value_zat)
        .unwrap_or_else(|_| zally_core::Zatoshis::zero());
    crate::received_note::ShieldedReceiveRecord {
        pool,
        value: amount,
        tx_id: row.tx_id,
        output_index: row.output_index,
        mined_height: row.mined_height,
        block_timestamp_ms: row.block_timestamp_ms,
        is_change: row.is_change,
        spent_our_inputs: row.spent_our_inputs,
    }
}

fn translate_unspent_note(
    row: zally_storage::UnspentShieldedNoteRow,
    observed_tip: Option<BlockHeight>,
) -> crate::unspent_note::UnspentShieldedNote {
    let pool = match row.protocol {
        zcash_protocol::ShieldedProtocol::Sapling => zally_chain::ShieldedPool::Sapling,
        zcash_protocol::ShieldedProtocol::Orchard => zally_chain::ShieldedPool::Orchard,
    };
    let amount = zally_core::Zatoshis::try_from(row.value_zat)
        .unwrap_or_else(|_| zally_core::Zatoshis::zero());
    let confirmations = match observed_tip {
        Some(tip) if tip.as_u32() >= row.mined_height.as_u32() => tip
            .as_u32()
            .saturating_sub(row.mined_height.as_u32())
            .saturating_add(1),
        _ => 0,
    };
    crate::unspent_note::UnspentShieldedNote {
        pool,
        value: amount,
        tx_id: row.tx_id,
        output_index: row.output_index,
        mined_height: row.mined_height,
        confirmations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<Wallet>();
    }
}
