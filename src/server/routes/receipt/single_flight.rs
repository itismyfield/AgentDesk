//! Per-key single-flight for the dashboard aggregate collectors.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::Mutex as AsyncMutex;

/// Async locks keyed by a resolved period for the token-analytics and receipt
/// collectors; idle locks are dropped so the map only holds keys in use.
#[derive(Default)]
pub(super) struct KeyedLocks {
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl KeyedLocks {
    fn lock_for(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        // Clones only happen under this map lock, so a count of 1 means no task
        // holds or waits on that lock and a later caller can start a fresh one.
        locks.retain(|held, lock| held == key || Arc::strong_count(lock) > 1);
        Arc::clone(locks.entry(key.to_string()).or_default())
    }

    /// Runs `work` under the key's lock in a detached task that outlives a dropped
    /// caller (client abort); the lock is released when the task ends or panics.
    pub(super) async fn run_detached<T, Work>(
        &self,
        key: &str,
        work: Work,
    ) -> Result<T, tokio::task::JoinError>
    where
        T: Send + 'static,
        Work: Future<Output = T> + Send + 'static,
    {
        let lock = self.lock_for(key);
        tokio::spawn(async move {
            let _guard = lock.lock().await;
            work.await
        })
        .await
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_keys_are_dropped_from_the_lock_map() {
        let locks = KeyedLocks::default();
        for day in 0..50 {
            let key = format!("today@{day}");
            assert_eq!(
                locks.run_detached(&key, async move { day }).await.unwrap(),
                day
            );
        }
        locks.lock_for("month@0");
        assert_eq!(locks.len(), 1, "finished keys must not accumulate");
    }
}
