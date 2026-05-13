//! Operator-facing wallet handle.

use std::collections::BTreeSet;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::broadcast;
use zally_core::{AccountId, BlockHeight, Network};
use zally_keys::{Mnemonic, SealingError, SeedMaterial, SeedSealing};
use zally_storage::{StorageError, WalletStorage};
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
/// methods are cancellation-safe. Zally v1 commits to one account per wallet (see RFC-0001
/// §10); the `AccountId` returned by [`Wallet::create`] and [`Wallet::open`] names that
/// single account.
#[derive(Clone)]
pub struct Wallet {
    pub(crate) inner: Arc<WalletInner>,
}

pub(crate) struct WalletInner {
    pub(crate) network: Network,
    #[allow(
        dead_code,
        reason = "Slice 1 does not call back into the sealing after wallet construction; \
                  Slice 3 uses it at signing time"
    )]
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
        let account_id = storage.create_account_for_seed(&seed, birthday).await?;

        let capabilities = build_capabilities(network, sealing_capability, storage_capability);
        emit_plaintext_warning_if_needed(&capabilities, "create");

        let (event_tx, _rx_keepalive) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let inner = Arc::new(WalletInner {
            network,
            sealing: Box::new(sealing),
            storage: Box::new(storage),
            base_capabilities: capabilities,
            event_tx,
            retry_policy: Mutex::new(RetryPolicy::default_v1()),
            circuit_breaker: CircuitBreaker::new(CircuitBreakerConfig::default_v1()),
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
            retry_policy: Mutex::new(RetryPolicy::default_v1()),
            circuit_breaker: CircuitBreaker::new(CircuitBreakerConfig::default_v1()),
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
    features.insert(Capability::EventStream);
    features.insert(Capability::IdempotentSend);
    features.insert(Capability::PcztV06);
    features.insert(Capability::MetricsSnapshot);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<Wallet>();
    }
}
