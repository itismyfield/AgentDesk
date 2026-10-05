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

/// Dormant queue barrier only: B2 population sealing is still required before Move/start.
#[allow(dead_code)]
pub(crate) struct FrozenQueueTransition {
    pub(crate) closing: Arc<super::input_runtime::fence::Closing>,
    _session: tokio::sync::OwnedMutexGuard<()>,
}
impl SharedData {
    #[allow(dead_code)]
    pub(crate) async fn freeze_legacy_queue(
        &self,
        closing: Arc<super::input_runtime::fence::Closing>,
        persistence: crate::services::turn_orchestrator::QueuePersistenceContext,
    ) -> Result<FrozenQueueTransition, super::input_runtime::fence::Failure> {
        closing.drain().await;
        let channel = ChannelId::new(closing.channel());
        let session = self
            .acquire_session_transition(channel)
            .await
            .map_err(|_| super::input_runtime::fence::Failure::Busy)?;
        let ack = self
            .mailbox(channel)
            .freeze_input(closing.clone(), persistence)
            .await?;
        closing.freeze(ack)?;
        Ok(FrozenQueueTransition {
            closing,
            _session: session,
        })
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

#[cfg(test)]
mod input_fence_contract_tests {
    use super::super::input_runtime::fence::{Failure, Gate, Mode};
    use super::*;
    use crate::services::provider::ProviderKind;
    use crate::services::turn_orchestrator::QueuePersistenceContext;

    struct Env(Option<std::ffi::OsString>);
    impl Drop for Env {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(old) => std::env::set_var("AGENTDESK_ROOT_DIR", old),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }
    #[test]
    fn input_fence_session_helper_drains_before_lock_and_holds_lock_through_ack() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let root = tempfile::tempdir().unwrap();
        let _env = Env(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = super::super::make_shared_data_for_tests();
                let channel = ChannelId::new(6_325_401);
                let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
                let permit = gate.admit().unwrap();
                let closing = Arc::new(gate.close().unwrap());
                let lock = shared.session_transition_lock(channel);
                let work = shared.freeze_legacy_queue(
                    closing.clone(),
                    QueuePersistenceContext::new(&ProviderKind::Claude, "session", None),
                );
                tokio::pin!(work);
                assert!(futures::poll!(work.as_mut()).is_pending());
                let held = lock
                    .try_lock()
                    .expect("drain must not hold the session mutex");
                assert!(
                    !root.path().join("runtime/discord_inflight").exists(),
                    "no prepare before drain"
                );
                drop(held);
                drop(permit);
                let transition = work.await.unwrap();
                assert_eq!(transition.closing.channel(), channel.get());
                assert_eq!(gate.mode(), Mode::Frozen);
                assert!(lock.try_lock().is_err(), "owned guard survives the ACK");
                drop(transition);
                assert!(lock.try_lock().is_ok());
            });
    }

    #[tokio::test(start_paused = true)]
    async fn input_fence_session_guard_timeout_never_prepares() {
        let shared = super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(6_325_402);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let closing = Arc::new(gate.close().unwrap());
        let held = shared.session_transition_lock(channel).lock_owned().await;
        let work = shared.freeze_legacy_queue(
            closing,
            QueuePersistenceContext::new(&ProviderKind::Claude, "session", None),
        );
        tokio::pin!(work);
        assert!(futures::poll!(work.as_mut()).is_pending());
        assert!(shared.mailboxes.peek(channel).is_none());
        tokio::time::advance(SESSION_TRANSITION_LOCK_WAIT_TIMEOUT).await;
        assert!(matches!(work.await, Err(Failure::Busy)));
        assert!(shared.mailboxes.peek(channel).is_none());
        assert_eq!(gate.mode(), Mode::Closing);
        drop(held);
    }
}
