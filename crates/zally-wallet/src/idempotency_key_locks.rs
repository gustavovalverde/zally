//! Serialization of sends that share an idempotency key.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use zally_core::IdempotencyKey;

/// One lock per idempotency key in use, so two sends with the same key cannot both build a
/// transaction: the second waits, then finds the transaction the first recorded.
///
/// The lock covers sends through one [`crate::Wallet`] handle and its clones, which is the
/// only writer of its wallet database.
#[derive(Default)]
pub(crate) struct IdempotencyKeyLocks {
    locks: Mutex<HashMap<IdempotencyKey, Arc<AsyncMutex<()>>>>,
}

impl IdempotencyKeyLocks {
    /// Waits for exclusive use of `key` and holds it until the guard drops.
    pub(crate) async fn lock(&self, key: &IdempotencyKey) -> OwnedMutexGuard<()> {
        let key_lock = {
            let mut locks = self.locks.lock();
            locks.retain(|_, held| Arc::strong_count(held) > 1);
            Arc::clone(locks.entry(key.clone()).or_default())
        };
        key_lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::IdempotencyKeyLocks;
    use zally_core::IdempotencyKey;

    #[tokio::test]
    async fn a_held_key_blocks_the_same_key_but_not_another() -> Result<(), &'static str> {
        let locks = IdempotencyKeyLocks::default();
        let key = IdempotencyKey::try_from("payout-1").map_err(|_| "valid key")?;
        let other = IdempotencyKey::try_from("payout-2").map_err(|_| "valid key")?;
        let held = locks.lock(&key).await;
        let wait = Duration::from_millis(50);
        assert!(
            tokio::time::timeout(wait, locks.lock(&key)).await.is_err(),
            "the same key waits while it is held"
        );
        assert!(
            tokio::time::timeout(wait, locks.lock(&other)).await.is_ok(),
            "another key is not blocked"
        );
        drop(held);
        assert!(
            tokio::time::timeout(wait, locks.lock(&key)).await.is_ok(),
            "the key is free once its guard drops"
        );
        Ok(())
    }
}
