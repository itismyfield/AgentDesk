use super::{ChannelId, SharedData};
use std::sync::Arc;

pub(crate) const SESSION_TRANSITION_LOCK_WAIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionTransitionBusy;

/// The exact channel lock held by a claim; a borrowed guard cannot release its caller's lock.
pub(crate) struct SessionTransitionLease<'a> {
    shared: &'a SharedData,
    channel: ChannelId,
    hold: TransitionHold<'a>,
}

enum TransitionHold<'a> {
    Owned(tokio::sync::OwnedMutexGuard<()>),
    Borrowed(&'a tokio::sync::OwnedMutexGuard<()>),
}

impl<'a> SessionTransitionLease<'a> {
    pub(crate) fn try_acquire(
        shared: &'a SharedData,
        channel: ChannelId,
    ) -> Result<Self, SessionTransitionBusy> {
        let guard = shared
            .session_transition_lock(channel)
            .try_lock_owned()
            .map_err(|_| SessionTransitionBusy)?;
        Ok(Self {
            shared,
            channel,
            hold: TransitionHold::Owned(guard),
        })
    }

    pub(crate) fn borrow(
        shared: &'a SharedData,
        channel: ChannelId,
        guard: &'a tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<Self, SessionTransitionBusy> {
        if !Arc::ptr_eq(
            tokio::sync::OwnedMutexGuard::mutex(guard),
            &shared.session_transition_lock(channel),
        ) {
            return Err(SessionTransitionBusy);
        }
        Ok(Self {
            shared,
            channel,
            hold: TransitionHold::Borrowed(guard),
        })
    }

    pub(crate) fn matches(&self, shared: &SharedData, channel: ChannelId) -> bool {
        let guard = match &self.hold {
            TransitionHold::Owned(guard) => guard,
            TransitionHold::Borrowed(guard) => guard,
        };
        std::ptr::eq(self.shared, shared)
            && self.channel == channel
            && Arc::ptr_eq(
                tokio::sync::OwnedMutexGuard::mutex(guard),
                &shared.session_transition_lock(channel),
            )
    }
}

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

    #[test]
    fn transition_lease_excludes_a_second_claim_until_it_is_dropped() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(4_794_903);
        let lease = SessionTransitionLease::try_acquire(&shared, channel).unwrap();
        assert!(lease.matches(&shared, channel));
        assert!(SessionTransitionLease::try_acquire(&shared, channel).is_err());
        drop(lease);
        assert!(SessionTransitionLease::try_acquire(&shared, channel).is_ok());
    }

    #[test]
    fn borrowed_transition_requires_the_same_channel_and_shared_owner() {
        let shared = make_shared_data_for_tests();
        let other = make_shared_data_for_tests();
        let channel = ChannelId::new(4_794_904);
        let guard = shared
            .session_transition_lock(channel)
            .try_lock_owned()
            .unwrap();
        let lease = SessionTransitionLease::borrow(&shared, channel, &guard).unwrap();
        assert!(lease.matches(&shared, channel));
        assert!(!lease.matches(&other, channel));
        assert!(!lease.matches(&shared, ChannelId::new(4_794_905)));
        assert!(SessionTransitionLease::borrow(&other, channel, &guard).is_err());
        assert!(
            SessionTransitionLease::borrow(&shared, ChannelId::new(4_794_905), &guard).is_err()
        );
        drop(lease);
        assert!(SessionTransitionLease::try_acquire(&shared, channel).is_err());
        drop(guard);
        assert!(SessionTransitionLease::try_acquire(&shared, channel).is_ok());
    }

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
