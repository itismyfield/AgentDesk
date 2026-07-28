use super::{ChannelId, SharedData};
use std::sync::Arc;

pub(crate) const SESSION_TRANSITION_LOCK_WAIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionTransitionBusy;

impl SharedData {
    pub(crate) fn session_transition_lock(
        &self,
        channel_id: ChannelId,
    ) -> Arc<tokio::sync::Mutex<()>> {
        if let Some(lock) = self
            .session_transition_locks
            .get(&channel_id)
            .and_then(|lock| lock.upgrade())
        {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        match self.session_transition_locks.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                if let Some(existing) = entry.get().upgrade() {
                    existing
                } else {
                    entry.insert(Arc::downgrade(&lock));
                    lock
                }
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(Arc::downgrade(&lock));
                // Prune only on registry growth. The common existing-channel path
                // above remains O(1), while dead Weak entries are still bounded by
                // subsequent vacant insertions.
                self.session_transition_locks
                    .retain(|_, candidate| candidate.strong_count() > 0);
                lock
            }
        }
    }

    pub(crate) async fn acquire_session_transition(
        &self,
        channel_id: ChannelId,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, SessionTransitionBusy> {
        tokio::time::timeout(
            SESSION_TRANSITION_LOCK_WAIT_TIMEOUT,
            self.session_transition_lock(channel_id).lock_owned(),
        )
        .await
        .map_err(|_| SessionTransitionBusy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::make_shared_data_for_tests;

    #[tokio::test(start_paused = true)]
    async fn transition_acquisition_times_out_after_contract_window() {
        let shared = make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_794_899);
        let held = shared
            .session_transition_lock(channel_id)
            .lock_owned()
            .await;
        let waiting_shared = Arc::clone(&shared);
        let waiter =
            tokio::spawn(
                async move { waiting_shared.acquire_session_transition(channel_id).await },
            );

        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        tokio::time::advance(SESSION_TRANSITION_LOCK_WAIT_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert!(
            waiter.is_finished(),
            "transition wait must end at the configured three-second boundary"
        );
        assert!(matches!(waiter.await.unwrap(), Err(SessionTransitionBusy)));
        drop(held);
    }

    #[test]
    fn inactive_channel_locks_are_pruned_only_on_vacant_insertion() {
        let shared = make_shared_data_for_tests();
        let old_channel = ChannelId::new(4_794_900);
        let live_channel = ChannelId::new(4_794_901);
        let new_channel = ChannelId::new(4_794_902);

        let old = shared.session_transition_lock(old_channel);
        let live = shared.session_transition_lock(live_channel);
        drop(old);

        let same_live = shared.session_transition_lock(live_channel);
        assert!(Arc::ptr_eq(&live, &same_live));
        assert!(
            shared.session_transition_locks.contains_key(&old_channel),
            "the O(1) existing-channel path must not scan and prune the registry"
        );

        let _new = shared.session_transition_lock(new_channel);
        assert!(!shared.session_transition_locks.contains_key(&old_channel));
        assert!(shared.session_transition_locks.contains_key(&live_channel));
        assert!(shared.session_transition_locks.contains_key(&new_channel));
    }
}
