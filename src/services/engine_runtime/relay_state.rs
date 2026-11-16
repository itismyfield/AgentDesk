//! Pure watcher registry, frontier and lease state.

pub(in crate::services) use super::lease;
pub(in crate::services) use registry::RegistryState;

pub(in crate::services) mod registry {
    use self::fences::commit_under_delivery_fence;
    #[cfg(test)]
    pub(in crate::services) use self::fences::set_delivery_fence_permitted_hook_for_tests;
    pub(in crate::services) use self::fences::{TerminalDeliveryFence, WatcherIdentityFence};
    use super::lease::DeliveryLeaseCell;
    use poise::serenity_prelude::ChannelId;
    use std::sync::Arc;

    pub(in crate::services) type IdentityProbe =
        fn(crate::config::ExecutionIdentityMode, &'static str, &str, Option<&str>) -> bool;
    #[derive(Clone, Copy)]
    pub(in crate::services) struct RegistryObservers {
        pub(in crate::services) identity: IdentityProbe,
        pub(in crate::services) bump_owner: fn(&TmuxWatcherRegistryGuard),
    }
    pub(in crate::services) type TmuxWatcherRegistryGuard = std::sync::MutexGuard<'static, ()>;
    pub(in crate::services) struct TmuxWatcherHandle {
        pub(in crate::services) tmux_session_name: String,

        pub(in crate::services) output_path: String,

        pub(in crate::services) paused: Arc<std::sync::atomic::AtomicBool>,

        pub(in crate::services) resume_offset: Arc<std::sync::Mutex<Option<u64>>>,

        pub(in crate::services) cancel: Arc<std::sync::atomic::AtomicBool>,

        pub(in crate::services) pause_epoch: Arc<std::sync::atomic::AtomicU64>,

        pub(in crate::services) turn_delivered: Arc<std::sync::atomic::AtomicBool>,

        pub(in crate::services) last_heartbeat_ts_ms: Arc<std::sync::atomic::AtomicI64>,
    }

    #[derive(Clone)]
    pub(in crate::services) struct ReservationRecord {
        pub(in crate::services) owner_channel_id: ChannelId,
        pub(in crate::services) cancel: Arc<std::sync::atomic::AtomicBool>,
        pub(in crate::services) cell: Arc<DeliveryLeaseCell>,
    }

    struct UnpairedWatcher {
        owner_channel_id: ChannelId,
        handle: TmuxWatcherHandle,
        reservation: Option<ReservationRecord>,
    }
    impl TmuxWatcherHandle {
        pub(in crate::services) fn heartbeat_stale(&self) -> bool {
            let last = self
                .last_heartbeat_ts_ms
                .load(std::sync::atomic::Ordering::Acquire);
            last <= 0
                || tmux_watcher_now_ms().saturating_sub(last) > TMUX_WATCHER_STALE_HEARTBEAT_MS
        }
    }
    pub(in crate::services) const TMUX_WATCHER_STALE_HEARTBEAT_MS: i64 = 60_000;
    pub(in crate::services) fn tmux_watcher_now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    pub(in crate::services) struct RegistryState {
        pub(in crate::services) by_tmux_session: dashmap::DashMap<String, TmuxWatcherHandle>,
        tmux_session_by_channel: dashmap::DashMap<ChannelId, String>,
        owner_channel_by_tmux_session: dashmap::DashMap<String, ChannelId>,

        restored_owner_by_tmux_session: dashmap::DashMap<String, ChannelId>,
        reservation_by_tmux_session: dashmap::DashMap<String, ReservationRecord>,
    }

    impl RegistryState {
        pub(in crate::services) fn new() -> Self {
            Self {
                by_tmux_session: dashmap::DashMap::new(),
                tmux_session_by_channel: dashmap::DashMap::new(),
                owner_channel_by_tmux_session: dashmap::DashMap::new(),
                restored_owner_by_tmux_session: dashmap::DashMap::new(),
                reservation_by_tmux_session: dashmap::DashMap::new(),
            }
        }
        pub(in crate::services) fn watcher_reservation_matches(
            &self,
            tmux_session_name: &str,
            owner_channel_id: ChannelId,
            expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
        ) -> bool {
            let record = self
                .reservation_by_tmux_session
                .get(tmux_session_name)
                .map(|entry| entry.value().clone());
            let Some(record) = record else {
                return false;
            };
            record.owner_channel_id == owner_channel_id
                && Arc::ptr_eq(&record.cancel, expected_cancel)
                && record.cell.watcher_reservation_matches(expected_cancel)
        }

        #[cfg(test)]
        pub(in crate::services) fn paired_reservation_matches_for_test(
            &self,
            tmux_session_name: &str,
            owner_channel_id: ChannelId,
            expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
        ) -> bool {
            self.watcher_reservation_matches(tmux_session_name, owner_channel_id, expected_cancel)
        }

        #[cfg(test)]
        pub(in crate::services) fn replace_paired_cancel_for_test(
            &self,
            tmux_session_name: &str,
            cancel: Arc<std::sync::atomic::AtomicBool>,
        ) {
            self.reservation_by_tmux_session
                .get_mut(tmux_session_name)
                .expect("paired reservation")
                .cancel = cancel;
        }

        #[cfg(test)]
        pub(in crate::services) fn remove_pair_without_payload_for_test(
            &self,
            observers: RegistryObservers,
            guard: &TmuxWatcherRegistryGuard,
            tmux_session_name: &str,
        ) -> Option<ReservationRecord> {
            self.unpair_locked(observers, guard, tmux_session_name)?
                .reservation
        }

        #[cfg(test)]
        pub(in crate::services) fn clear_reservation_for_test(
            &self,
            guard: &TmuxWatcherRegistryGuard,
            record: ReservationRecord,
        ) -> bool {
            record.cell.clear_watcher_reservation(guard, &record.cancel)
        }

        fn clear_reservation(&self, guard: &TmuxWatcherRegistryGuard, record: ReservationRecord) {
            record.cell.clear_watcher_reservation(guard, &record.cancel);
        }

        fn unpair_locked(
            &self,
            observers: RegistryObservers,
            guard: &TmuxWatcherRegistryGuard,
            tmux_session_name: &str,
        ) -> Option<UnpairedWatcher> {
            let owner_channel_id = self
                .owner_channel_by_tmux_session
                .get(tmux_session_name)
                .map(|entry| *entry.value())?;
            let channel_matches = self
                .tmux_session_by_channel
                .get(&owner_channel_id)
                .is_some_and(|entry| entry.value().as_str() == tmux_session_name);
            if !channel_matches
                || !self.by_tmux_session.contains_key(tmux_session_name)
                || !self
                    .reservation_by_tmux_session
                    .contains_key(tmux_session_name)
            {
                return None;
            }

            let (_, handle) = self.by_tmux_session.remove(tmux_session_name)?;
            let (_, reservation) = self.reservation_by_tmux_session.remove(tmux_session_name)?;
            self.owner_channel_by_tmux_session.remove(tmux_session_name);
            self.tmux_session_by_channel.remove(&owner_channel_id);
            (observers.bump_owner)(guard);
            Some(UnpairedWatcher {
                owner_channel_id,
                handle,
                reservation: Some(reservation),
            })
        }

        pub(in crate::services) fn len(&self) -> usize {
            self.by_tmux_session.len()
        }

        pub(in crate::services) fn contains_key(&self, channel_id: &ChannelId) -> bool {
            self.channel_binding(channel_id)
                .and_then(|binding| self.by_tmux_session.get(&binding.tmux_session_name))
                .is_some()
        }

        pub(in crate::services) fn get(
            &self,
            channel_id: &ChannelId,
        ) -> Option<dashmap::mapref::one::Ref<'_, String, TmuxWatcherHandle>> {
            let tmux_session_name = self.tmux_session_by_channel.get(channel_id)?.clone();
            self.by_tmux_session.get(&tmux_session_name)
        }

        pub(in crate::services) fn insert_locked(
            &self,
            observers: RegistryObservers,
            guard: &TmuxWatcherRegistryGuard,
            channel_id: ChannelId,
            handle: TmuxWatcherHandle,
            resolve_delivery_cell: impl FnOnce(ChannelId) -> Arc<DeliveryLeaseCell>,
        ) -> Option<TmuxWatcherHandle> {
            let tmux_session_name = handle.tmux_session_name.clone();
            let cancel = Arc::clone(&handle.cancel);
            let cell = resolve_delivery_cell(channel_id);
            let channel_session_key = tmux_session_name.clone();
            let owner_session_key = tmux_session_name.clone();
            let handle_session_key = tmux_session_name.clone();
            let reservation_session_key = tmux_session_name.clone();
            let new_record = ReservationRecord {
                owner_channel_id: channel_id,
                cancel: Arc::clone(&cancel),
                cell: Arc::clone(&cell),
            };
            let old_channel_session = self
                .tmux_session_by_channel
                .get(&channel_id)
                .map(|entry| entry.clone());
            let old_session_owner = self
                .owner_channel_by_tmux_session
                .get(&tmux_session_name)
                .map(|entry| *entry.value());

            let mut channel_displaced = old_channel_session
                .as_deref()
                .and_then(|session| self.unpair_locked(observers, guard, session));
            let same_row = old_channel_session.as_deref() == Some(tmux_session_name.as_str());
            let mut session_displaced = (!same_row && old_session_owner.is_some())
                .then(|| self.unpair_locked(observers, guard, &tmux_session_name))
                .flatten();
            for old in [&mut channel_displaced, &mut session_displaced] {
                if let Some(record) = old.as_mut().and_then(|old| old.reservation.take()) {
                    self.clear_reservation(guard, record);
                }
            }

            self.restored_owner_by_tmux_session
                .remove(&tmux_session_name);
            self.tmux_session_by_channel
                .insert(channel_id, channel_session_key);
            self.owner_channel_by_tmux_session
                .insert(owner_session_key, channel_id);
            self.by_tmux_session.insert(handle_session_key, handle);
            self.reservation_by_tmux_session
                .insert(reservation_session_key, new_record);
            cell.install_watcher_reservation(guard, cancel);
            (observers.bump_owner)(guard);
            session_displaced
                .or(channel_displaced)
                .map(|old| old.handle)
        }

        pub(in crate::services) fn remove_locked(
            &self,
            observers: RegistryObservers,
            guard: &TmuxWatcherRegistryGuard,
            channel_id: &ChannelId,
        ) -> Option<(ChannelId, TmuxWatcherHandle)> {
            let tmux_session_name = self
                .tmux_session_by_channel
                .get(channel_id)
                .map(|entry| entry.clone())?;
            let mut removed = self.unpair_locked(observers, guard, &tmux_session_name)?;
            if let Some(record) = removed.reservation.take() {
                self.clear_reservation(guard, record);
            }
            Some((removed.owner_channel_id, removed.handle))
        }

        pub(in crate::services) fn remove_tmux_session_locked(
            &self,
            observers: RegistryObservers,
            guard: &TmuxWatcherRegistryGuard,
            tmux_session_name: &str,
        ) -> Option<(ChannelId, TmuxWatcherHandle)> {
            let mut removed = self.unpair_locked(observers, guard, tmux_session_name)?;
            if let Some(record) = removed.reservation.take() {
                self.clear_reservation(guard, record);
            }
            Some((removed.owner_channel_id, removed.handle))
        }

        pub(in crate::services) fn remove_tmux_session_if_current(
            &self,
            observers: RegistryObservers,
            guard: &TmuxWatcherRegistryGuard,
            tmux_session_name: &str,
            expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
            identity: Option<&WatcherIdentityFence>,
            delivery: Option<&TerminalDeliveryFence>,
        ) -> Option<(ChannelId, TmuxWatcherHandle)> {
            let live_output_path = self
                .by_tmux_session
                .get(tmux_session_name)
                .filter(|entry| Arc::ptr_eq(&entry.cancel, expected_cancel))
                .map(|entry| entry.output_path.clone())?;
            if let Some(identity) = identity
                && !identity.permits_destruction(
                    observers.identity,
                    self.owner_channel_for_tmux_session(tmux_session_name),
                    &live_output_path,
                )
            {
                return None;
            }

            let mut removed = commit_under_delivery_fence(delivery, || {
                self.unpair_locked(observers, guard, tmux_session_name)
            })
            .flatten()?;
            if let Some(record) = removed.reservation.take() {
                self.clear_reservation(guard, record);
            }
            Some((removed.owner_channel_id, removed.handle))
        }

        pub(in crate::services) fn cancel_and_remove_channel_if_current(
            &self,
            guard: &TmuxWatcherRegistryGuard,
            channel_id: &ChannelId,
            expected_tmux_session_name: &str,
            expected_output_path: &str,
            expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
            fences: (
                RegistryObservers,
                Option<&WatcherIdentityFence>,
                Option<&TerminalDeliveryFence>,
            ),
        ) -> bool {
            let (observers, identity, delivery) = fences;
            let Some(tmux_session_name) = self
                .tmux_session_by_channel
                .get(channel_id)
                .map(|entry| entry.clone())
            else {
                return false;
            };
            if tmux_session_name != expected_tmux_session_name {
                return false;
            }
            let matches_current =
                self.by_tmux_session
                    .get(&tmux_session_name)
                    .is_some_and(|entry| {
                        entry.output_path == expected_output_path
                            && Arc::ptr_eq(&entry.cancel, expected_cancel)
                    });
            if !matches_current {
                return false;
            }
            if let Some(identity) = identity
                && !identity.permits_destruction(
                    observers.identity,
                    self.owner_channel_for_tmux_session(&tmux_session_name),
                    expected_output_path,
                )
            {
                return false;
            }

            let removed = commit_under_delivery_fence(delivery, || {
                let removed = self.unpair_locked(observers, guard, &tmux_session_name);
                if let Some(removed) = &removed {
                    removed
                        .handle
                        .cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                removed
            })
            .flatten();
            let Some(mut removed) = removed else {
                return false;
            };
            if let Some(record) = removed.reservation.take() {
                self.clear_reservation(guard, record);
            }
            true
        }

        pub(in crate::services) fn iter(
            &self,
        ) -> dashmap::iter::Iter<'_, String, TmuxWatcherHandle> {
            self.by_tmux_session.iter()
        }

        pub(in crate::services) fn restore_owner_channel_for_tmux_session(
            &self,
            observers: RegistryObservers,
            guard: &TmuxWatcherRegistryGuard,
            tmux_session_name: &str,
            channel_id: ChannelId,
        ) -> bool {
            if self
                .owner_channel_by_tmux_session
                .contains_key(tmux_session_name)
            {
                self.clear_restored_owner_locked(observers, guard, tmux_session_name);
                return false;
            }
            let changed = self
                .restored_owner_by_tmux_session
                .get(tmux_session_name)
                .map(|entry| *entry.value())
                != Some(channel_id);
            self.restored_owner_by_tmux_session
                .insert(tmux_session_name.to_string(), channel_id);
            if changed {
                (observers.bump_owner)(guard);
            }
            changed
        }

        pub(in crate::services) fn clear_restored_owner_locked(
            &self,
            observers: RegistryObservers,
            guard: &TmuxWatcherRegistryGuard,
            tmux_session_name: &str,
        ) {
            if self
                .restored_owner_by_tmux_session
                .remove(tmux_session_name)
                .is_some()
            {
                (observers.bump_owner)(guard);
            }
        }

        pub(in crate::services) fn retain_owner_during_session_rebind(
            &self,
            observers: RegistryObservers,
            guard: &TmuxWatcherRegistryGuard,
            tmux_session_name: &str,
            channel_id: ChannelId,
        ) -> bool {
            let tmux_session_name = tmux_session_name.trim();
            if tmux_session_name.is_empty() {
                return false;
            }
            if let Some(current_owner) = self.owner_channel_by_tmux_session.get(tmux_session_name)
                && *current_owner.value() != channel_id
            {
                return false;
            }
            let previous = self
                .restored_owner_by_tmux_session
                .insert(tmux_session_name.to_string(), channel_id);
            if previous != Some(channel_id) {
                (observers.bump_owner)(guard);
            }
            true
        }

        pub(in crate::services) fn has_live_watcher_handle(&self, tmux_session_name: &str) -> bool {
            self.by_tmux_session.contains_key(tmux_session_name)
        }

        pub(in crate::services) fn tmux_session_is_stale(
            &self,
            tmux_session_name: &str,
        ) -> Option<bool> {
            self.by_tmux_session
                .get(tmux_session_name)
                .map(|entry| entry.heartbeat_stale())
        }

        pub(in crate::services) fn tmux_session_live_for_relay(
            &self,
            tmux_session_name: &str,
        ) -> Option<bool> {
            self.by_tmux_session.get(tmux_session_name).map(|entry| {
                !entry.cancel.load(std::sync::atomic::Ordering::Relaxed) && !entry.heartbeat_stale()
            })
        }

        pub(in crate::services) fn watcher_output_path(
            &self,
            tmux_session_name: &str,
        ) -> Option<String> {
            self.by_tmux_session
                .get(tmux_session_name)
                .map(|entry| entry.output_path.clone())
        }
    }

    mod bindings {
        use std::collections::BTreeSet;

        use super::{ChannelId, RegistryState as TmuxWatcherRegistry};

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub(in crate::services) struct TmuxWatcherBinding {
            pub(in crate::services) owner_channel_id: ChannelId,
            pub(in crate::services) tmux_session_name: String,

            pub(in crate::services) output_path: Option<String>,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub(in crate::services) struct AuthoritativeSessionBinding {
            pub owner_channel_id: ChannelId,
            pub tmux_session_name: String,
        }

        impl TmuxWatcherRegistry {
            pub(in crate::services) fn authoritative_session_bindings(
                &self,
                owner_channel_id: ChannelId,
            ) -> Vec<AuthoritativeSessionBinding> {
                let mut names = BTreeSet::new();
                for owners in [
                    &self.owner_channel_by_tmux_session,
                    &self.restored_owner_by_tmux_session,
                ] {
                    names.extend(
                        owners
                            .iter()
                            .filter(|entry| *entry.value() == owner_channel_id)
                            .map(|entry| entry.key().clone()),
                    );
                }
                names
                    .into_iter()
                    .filter_map(|tmux_session_name| {
                        (self.owner_channel_for_tmux_session(&tmux_session_name)
                            == Some(owner_channel_id))
                        .then_some(AuthoritativeSessionBinding {
                            owner_channel_id,
                            tmux_session_name,
                        })
                    })
                    .collect()
            }

            pub(in crate::services) fn channel_binding(
                &self,
                channel_id: &ChannelId,
            ) -> Option<TmuxWatcherBinding> {
                let tmux_session_name = self.tmux_session_by_channel.get(channel_id)?.clone();
                let owner_channel_id = self
                    .owner_channel_by_tmux_session
                    .get(&tmux_session_name)
                    .map(|entry| *entry.value())
                    .unwrap_or(*channel_id);
                let output_path = self.watcher_output_path(&tmux_session_name);
                Some(TmuxWatcherBinding {
                    owner_channel_id,
                    tmux_session_name,
                    output_path,
                })
            }

            pub(in crate::services) fn owner_channel_for_tmux_session(
                &self,
                tmux_session_name: &str,
            ) -> Option<ChannelId> {
                self.owner_channel_by_tmux_session
                    .get(tmux_session_name)
                    .map(|entry| *entry.value())
                    .or_else(|| {
                        self.restored_owner_by_tmux_session
                            .get(tmux_session_name)
                            .map(|entry| *entry.value())
                    })
            }
        }
    }
    pub(in crate::services) use bindings::{AuthoritativeSessionBinding, TmuxWatcherBinding};

    #[cfg(test)]
    impl RegistryState {
        pub(in crate::services) fn has_restored_owner_for_tests(&self, name: &str) -> bool {
            self.restored_owner_by_tmux_session.contains_key(name)
        }

        pub(in crate::services) fn seed_binding_for_tests(
            &self,
            name: &str,
            restored: Option<ChannelId>,
            live: Option<ChannelId>,
        ) {
            if let Some(channel) = restored {
                self.restored_owner_by_tmux_session
                    .insert(name.into(), channel);
            }
            if let Some(channel) = live {
                self.owner_channel_by_tmux_session
                    .insert(name.into(), channel);
            }
        }
    }

    mod fences {
        use super::super::lease::{DeliveryLeaseKey, lease_now_ms};
        use super::*;
        use crate::config::ExecutionIdentityMode;
        struct PinnedWatcherBinding {
            owner_channel_id: ChannelId,
            output_path: String,
        }

        pub(in crate::services) struct WatcherIdentityFence {
            mode: ExecutionIdentityMode,

            site: &'static str,
            tmux_session_name: String,
            captured_spawn_nonce: Option<String>,
            pinned_binding: Option<PinnedWatcherBinding>,
        }

        impl WatcherIdentityFence {
            pub(in crate::services) fn from_capture(
                mode: ExecutionIdentityMode,
                site: &'static str,
                tmux_session_name: &str,
                captured_spawn_nonce: Option<String>,
            ) -> Self {
                Self {
                    mode,
                    site,
                    tmux_session_name: tmux_session_name.to_string(),
                    captured_spawn_nonce,
                    pinned_binding: None,
                }
            }

            pub(in crate::services) fn with_pinned_binding(
                mut self,
                owner_channel_id: ChannelId,
                output_path: &str,
            ) -> Self {
                self.pinned_binding = Some(PinnedWatcherBinding {
                    owner_channel_id,
                    output_path: output_path.to_string(),
                });
                self
            }

            pub(super) fn permits_destruction(
                &self,
                observe: super::IdentityProbe,
                live_owner_channel_id: Option<ChannelId>,
                live_output_path: &str,
            ) -> bool {
                let binding_permits =
                    self.permits_pinned_binding(live_owner_channel_id, live_output_path);
                let identity_permits = observe(
                    self.mode,
                    self.site,
                    &self.tmux_session_name,
                    self.captured_spawn_nonce.as_deref(),
                );
                binding_permits && identity_permits
            }

            fn permits_pinned_binding(
                &self,
                live_owner_channel_id: Option<ChannelId>,
                live_output_path: &str,
            ) -> bool {
                let Some(pinned) = self.pinned_binding.as_ref() else {
                    return true;
                };
                if live_owner_channel_id == Some(pinned.owner_channel_id)
                    && live_output_path == pinned.output_path
                {
                    return true;
                }
                if self.mode.records_identity_observations() {
                    tracing::info!(
                        counter = "execution_identity_binding_mismatch",
                        site = self.site,
                        session_key = self.tmux_session_name.as_str(),
                        pinned_owner_channel_id = pinned.owner_channel_id.get(),
                        live_owner_channel_id =
                            live_owner_channel_id.map(ChannelId::get).unwrap_or(0),
                        output_path_changed = live_output_path != pinned.output_path,
                        "pinned watcher binding no longer matches the live registry row"
                    );
                }
                !self.mode.denies_on_incarnation_mismatch()
            }
        }

        pub(in crate::services) struct TerminalDeliveryFence {
            lease: Arc<DeliveryLeaseCell>,
            expected_key: DeliveryLeaseKey,

            site: &'static str,
        }

        impl TerminalDeliveryFence {
            pub(in crate::services) fn capture(
                lease: Arc<DeliveryLeaseCell>,
                expected_key: DeliveryLeaseKey,
                site: &'static str,
            ) -> Self {
                Self {
                    lease,
                    expected_key,
                    site,
                }
            }

            fn commit_if_permitted<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
                self.lease.with_state_locked(|snapshot| {

            let now_ms = lease_now_ms();
            if let Some(deadline_ms) = snapshot
                .identity_matched(&self.expected_key)
                .and_then(|matched| matched.deadline_ms)
                && deadline_ms > now_ms
            {
                tracing::info!(
                    counter = "terminal_delivery_fence_veto",
                    site = self.site,
                    channel_id = self.expected_key.channel_id().get(),
                    deadline_ms,
                    now_ms,
                    "identity-matched delivery lease is still live; refusing the destructive watcher removal"
                );
                return None;
            }
            #[cfg(test)]
            run_delivery_fence_permitted_hook_for_tests(self.site);
            Some(commit())
        })
            }
        }

        pub(super) fn commit_under_delivery_fence<T>(
            delivery: Option<&TerminalDeliveryFence>,
            commit: impl FnOnce() -> T,
        ) -> Option<T> {
            match delivery {
                None => Some(commit()),
                Some(fence) => fence.commit_if_permitted(commit),
            }
        }

        #[cfg(test)]
        type DeliveryFencePermittedHook = Arc<dyn Fn(&'static str) + Send + Sync + 'static>;

        #[cfg(test)]
        static DELIVERY_FENCE_PERMITTED_HOOK: std::sync::LazyLock<
            std::sync::Mutex<Option<DeliveryFencePermittedHook>>,
        > = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

        #[cfg(test)]
        fn run_delivery_fence_permitted_hook_for_tests(site: &'static str) {
            let hook = DELIVERY_FENCE_PERMITTED_HOOK
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            if let Some(hook) = hook {
                hook(site);
            }
        }

        #[cfg(test)]
        pub(in crate::services) struct DeliveryFencePermittedHookGuard;

        #[cfg(test)]
        impl Drop for DeliveryFencePermittedHookGuard {
            fn drop(&mut self) {
                *DELIVERY_FENCE_PERMITTED_HOOK
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = None;
            }
        }

        #[cfg(test)]
        pub(in crate::services) fn set_delivery_fence_permitted_hook_for_tests(
            hook: DeliveryFencePermittedHook,
        ) -> DeliveryFencePermittedHookGuard {
            *DELIVERY_FENCE_PERMITTED_HOOK
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(hook);
            DeliveryFencePermittedHookGuard
        }
    }
}

use crate::services::cluster::stream_relay::source_read_proof::SourceCertificate;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
#[derive(Default)]
pub(in crate::services) struct FrontierResetState {
    incarnation: u64,
    active_mutations: usize,
    certificate: Option<SourceCertificate>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services) struct RelayFrontierToken {
    pub(in crate::services) reset_incarnation: u64,
    pub(in crate::services) committed_offset: u64,
}
#[cfg(all(test, unix))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrontierPublication {
    Published,
    Preserved,
    Unmeasured,
}

impl FrontierResetState {
    pub(in crate::services) fn admit_mutation(
        &mut self,
        token: RelayFrontierToken,
        confirmed: &AtomicU64,
    ) -> bool {
        if self.incarnation != token.reset_incarnation
            || confirmed.load(Ordering::Acquire) != token.committed_offset
        {
            return false;
        }
        self.active_mutations += 1;
        true
    }
    pub(in crate::services) fn admit_incarnation(&mut self, reset_incarnation: u64) -> bool {
        if self.incarnation != reset_incarnation {
            return false;
        }
        self.active_mutations += 1;
        true
    }
    pub(in crate::services) fn release_mutation(&mut self) {
        self.active_mutations -= 1;
    }

    pub(in crate::services) fn advance_untyped_frontier(
        &mut self,
        end: u64,
        generation: Option<i64>,
        confirmed: &AtomicU64,
        confirmed_generation: &AtomicI64,
        before_generation: impl FnOnce(),
    ) -> u64 {
        let current = confirmed.load(Ordering::Acquire);
        if current < end {
            self.certificate.take();
            confirmed.store(end, Ordering::Release);
            if let Some(generation) = generation {
                before_generation();
                confirmed_generation.store(generation, Ordering::Release);
            }
        }
        current.max(end)
    }

    pub(in crate::services) fn frontier_token(
        &mut self,
        confirmed: &AtomicU64,
        confirmed_generation: &AtomicI64,
    ) -> RelayFrontierToken {
        let committed_offset = confirmed.load(Ordering::Acquire);
        let generation = confirmed_generation.load(Ordering::Acquire);
        if self.certificate.as_ref().is_some_and(|certificate| {
            !certificate.matches(committed_offset, generation, self.incarnation)
        }) {
            self.certificate.take();
        }
        RelayFrontierToken {
            reset_incarnation: self.incarnation,
            committed_offset,
        }
    }

    pub(in crate::services) fn reset_confirmed_frontier_with_generation(
        &mut self,
        expected_offset: u64,
        new_offset: u64,
        generation: Option<i64>,
        confirmed: &AtomicU64,
        confirmed_generation: &AtomicI64,
        before_generation: impl FnOnce(),
    ) -> bool {
        debug_assert!(new_offset < expected_offset);
        if self.active_mutations != 0 {
            return false;
        }
        let reset = confirmed
            .compare_exchange(
                expected_offset,
                new_offset,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if reset {
            self.incarnation = self.incarnation.wrapping_add(1);
            self.certificate.take();
            if let Some(generation) = generation {
                before_generation();
                confirmed_generation.store(generation, Ordering::Release);
            }
        }
        reset
    }

    #[cfg(all(test, unix))]
    pub(in crate::services) fn publish_confirmed_for_tests(
        &mut self,
        end: u64,
        generation: i64,
        certificate: Option<SourceCertificate>,
        confirmed: &AtomicU64,
        confirmed_generation: &AtomicI64,
    ) -> FrontierPublication {
        let current = confirmed.load(Ordering::Acquire);
        if self.certificate.as_ref().is_some_and(|certificate| {
            !certificate.matches(
                current,
                confirmed_generation.load(Ordering::Acquire),
                self.incarnation,
            )
        }) {
            self.certificate.take();
        }
        if end < current {
            return if self.certificate.is_some() {
                FrontierPublication::Preserved
            } else {
                FrontierPublication::Unmeasured
            };
        }
        if current < end {
            self.certificate.take();
            confirmed.store(end, Ordering::Release);
            confirmed_generation.store(generation, Ordering::Release);
        }
        let current_generation = confirmed_generation.load(Ordering::Acquire);
        if let Some(certificate) = certificate
            .filter(|certificate| certificate.matches(end, current_generation, self.incarnation))
        {
            self.certificate = Some(certificate);
            FrontierPublication::Published
        } else {
            FrontierPublication::Unmeasured
        }
    }

    #[cfg(all(test, unix))]
    pub(in crate::services) fn frontier_tuple_for_tests(
        &self,
        confirmed: &AtomicU64,
        generation: &AtomicI64,
    ) -> (u64, i64, u64, bool) {
        (
            confirmed.load(Ordering::Acquire),
            generation.load(Ordering::Acquire),
            self.incarnation,
            self.certificate.is_some(),
        )
    }
    #[cfg(all(test, unix))]
    pub(in crate::services) fn certificate_for_tests(&self) -> Option<SourceCertificate> {
        self.certificate.clone()
    }
}
