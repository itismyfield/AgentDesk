use super::*;

// #5457: the S4 fence layer moved into `fences`. Every name a caller actually
// spells as `tmux_watcher_registry::<name>` is re-exported below, so those
// paths resolve unchanged. The two RAII guard types the `set_*_for_tests` pair
// returns are deliberately NOT re-exported: every caller binds them as
// `let _guard = set_...(..)` and never names the type, so re-exporting them
// would only add an import nothing uses.
mod fences;
mod owner_observation;
mod session_binding;

use owner_observation::bump_owner_observation_epoch;
#[cfg(test)]
pub(crate) use owner_observation::owner_publication_fence_held_for_tests;
pub(in crate::services::discord) use owner_observation::{
    owner_observation_epoch, with_owner_publication,
};

pub(in crate::services::discord) use session_binding::{
    AuthoritativeSessionBinding, TmuxWatcherBinding,
};

pub(in crate::services::discord) use fences::{
    TerminalDeliveryFence, WatcherIdentityFence, execution_identity_mode,
};
#[cfg(test)]
pub(in crate::services::discord) use fences::{
    set_delivery_fence_permitted_hook_for_tests, set_execution_identity_mode_for_tests,
};

#[cfg(test)]
pub(in crate::services::discord) use crate::services::engine_runtime::relay_state::registry::ReservationRecord;
use crate::services::engine_runtime::relay_state::registry::{RegistryObservers, RegistryState};
pub(in crate::services::discord) use crate::services::engine_runtime::relay_state::registry::{
    TMUX_WATCHER_STALE_HEARTBEAT_MS, TmuxWatcherHandle, tmux_watcher_now_ms,
};
pub(in crate::services::discord) type TmuxWatcherRegistryGuard = std::sync::MutexGuard<'static, ()>;

static TMUX_WATCHER_REGISTRY_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

pub(in crate::services::discord) fn lock_tmux_watcher_registry() -> TmuxWatcherRegistryGuard {
    TMUX_WATCHER_REGISTRY_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// #5071 relay-tail S4 r2 (P1-2 ②): the HALF-BUILT view
/// [`TmuxWatcherRegistry::under_identity_fence`] returns. It carries the
/// [`WatcherIdentityFence`] and deliberately exposes NO destructive method — the
/// only thing that can be done with it is
/// [`IdentityFencePendingDelivery::with_terminal_delivery_fence`], which
/// consumes it and yields the [`IdentityFencedRegistry`] that can actually
/// remove.
///
/// This is the type-level half of keeping the S4 conjunct attached. In r1 both
/// fences rode on one view with `delivery: Option<_>`, so deleting the single
/// `.with_terminal_delivery_fence(..)` line at a call site still compiled and
/// still removed — silently unfenced. Now that deletion does not typecheck. The
/// [`identity_fence_bind`/`delivery_fence_bind` pairing check][ratchet] in
/// `scripts/check_destructive_call_site_ratchet.py` is the lexical half, and
/// catches the same omission for anything the type system stops covering (a new
/// binder added in a file the compiler is happy with but reviewers should see).
///
/// [ratchet]: ../../../scripts/check_destructive_call_site_ratchet.py
pub(in crate::services::discord) struct IdentityFencePendingDelivery<'a> {
    registry: &'a TmuxWatcherRegistry,
    fence: WatcherIdentityFence,
}

impl<'a> IdentityFencePendingDelivery<'a> {
    /// Add the S4 delivery-lease conjunct and complete the view. Separate from
    /// [`TmuxWatcherRegistry::under_identity_fence`] so the two production
    /// callers stay the only `under_identity_fence` sites the destructive
    /// ratchet counts, and REQUIRED because a caller that has no lease
    /// coordinate to pin must be a compile error rather than a silently `None`
    /// argument.
    pub(in crate::services::discord) fn with_terminal_delivery_fence(
        self,
        delivery: TerminalDeliveryFence,
    ) -> IdentityFencedRegistry<'a> {
        IdentityFencedRegistry {
            registry: self.registry,
            fence: self.fence,
            delivery,
        }
    }
}

/// A registry view whose two CAS removals also require the
/// [`WatcherIdentityFence`] conjunct AND the [`TerminalDeliveryFence`] one. It
/// is unconstructible without both: the only way to reach it is
/// [`IdentityFencePendingDelivery::with_terminal_delivery_fence`].
///
/// The fenced methods keep the SAME names as their unfenced originals on
/// purpose: the #5071 T3-A4 destructive-call-site ratchet counts those names
/// lexically, so routing a destructive removal through this view must not make
/// the call site disappear from the inventory.
pub(in crate::services::discord) struct IdentityFencedRegistry<'a> {
    registry: &'a TmuxWatcherRegistry,
    fence: WatcherIdentityFence,
    delivery: TerminalDeliveryFence,
}

impl IdentityFencedRegistry<'_> {
    /// Only REMOVES — like the unfenced original it never writes `cancel`, so
    /// the caller stores it after a `Some` return.
    pub(in crate::services::discord) fn remove_tmux_session_if_current(
        &self,
        tmux_session_name: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        self.registry.remove_tmux_session_if_current_locked(
            &guard,
            tmux_session_name,
            expected_cancel,
            Some(&self.fence),
            Some(&self.delivery),
        )
    }

    /// Cancels internally, so a `true` return already means the removed
    /// handle's `cancel` was stored — and a `false` return means nothing was.
    pub(in crate::services::discord) fn cancel_and_remove_channel_if_current(
        &self,
        channel_id: &ChannelId,
        expected_tmux_session_name: &str,
        expected_output_path: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        let guard = lock_tmux_watcher_registry();
        self.registry.cancel_and_remove_channel_if_current_locked(
            &guard,
            channel_id,
            expected_tmux_session_name,
            expected_output_path,
            expected_cancel,
            (Some(&self.fence), Some(&self.delivery)),
        )
    }
}

pub(in crate::services) struct TmuxWatcherRegistry {
    storage: Arc<RegistryState>,
    tmux_relay_coords: Arc<dashmap::DashMap<ChannelId, Arc<TmuxRelayCoord>>>,
}
impl std::ops::Deref for TmuxWatcherRegistry {
    type Target = RegistryState;
    fn deref(&self) -> &Self::Target {
        &self.storage
    }
}
impl TmuxWatcherRegistry {
    #[cfg(test)]
    pub(in crate::services::discord) fn new_with_coords(
        tmux_relay_coords: Arc<dashmap::DashMap<ChannelId, Arc<TmuxRelayCoord>>>,
    ) -> Self {
        Self::from_storage(Arc::new(RegistryState::new()), tmux_relay_coords)
    }
    pub(in crate::services::discord) fn from_storage(
        storage: Arc<RegistryState>,
        tmux_relay_coords: Arc<dashmap::DashMap<ChannelId, Arc<TmuxRelayCoord>>>,
    ) -> Self {
        Self {
            storage,
            tmux_relay_coords,
        }
    }
    fn observers() -> RegistryObservers {
        RegistryObservers {
            identity: fences::observe_identity,
            bump_owner: bump_owner_observation_epoch,
        }
    }
    fn resolve_delivery_cell(&self, channel_id: ChannelId) -> Arc<DeliveryLeaseCell> {
        crate::services::discord::delivery_lease_cell::assert_payload_not_held();
        let coord = {
            let entry = self
                .tmux_relay_coords
                .entry(channel_id)
                .or_insert_with(|| Arc::new(TmuxRelayCoord::new(channel_id)));
            Arc::clone(entry.value())
        };
        Arc::clone(&coord.delivery_lease)
    }
    #[cfg(test)]
    pub(in crate::services::discord) fn relay_coords_share_identity_for_test(
        &self,
        coords: &Arc<dashmap::DashMap<ChannelId, Arc<TmuxRelayCoord>>>,
    ) -> bool {
        Arc::ptr_eq(&self.tmux_relay_coords, coords)
    }
    #[cfg(test)]
    pub(in crate::services::discord) fn insert(
        &self,
        channel_id: ChannelId,
        handle: TmuxWatcherHandle,
    ) -> Option<TmuxWatcherHandle> {
        let guard = lock_tmux_watcher_registry();
        self.insert_locked(&guard, channel_id, handle)
    }
    pub(in crate::services::discord) fn under_identity_fence(
        &self,
        fence: WatcherIdentityFence,
    ) -> IdentityFencePendingDelivery<'_> {
        IdentityFencePendingDelivery {
            registry: self,
            fence,
        }
    }

    pub(in crate::services::discord) fn insert_locked(
        &self,
        guard: &TmuxWatcherRegistryGuard,
        channel_id: ChannelId,
        handle: TmuxWatcherHandle,
    ) -> Option<TmuxWatcherHandle> {
        self.storage
            .insert_locked(Self::observers(), guard, channel_id, handle, |channel| {
                self.resolve_delivery_cell(channel)
            })
    }
    pub(in crate::services::discord) fn remove(
        &self,
        channel: &ChannelId,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        self.remove_locked(&guard, channel)
    }
    pub(in crate::services::discord) fn remove_locked(
        &self,
        guard: &TmuxWatcherRegistryGuard,
        channel: &ChannelId,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        self.storage
            .remove_locked(Self::observers(), guard, channel)
    }
    pub(in crate::services::discord) fn remove_tmux_session_locked(
        &self,
        guard: &TmuxWatcherRegistryGuard,
        session: &str,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        self.storage
            .remove_tmux_session_locked(Self::observers(), guard, session)
    }
    pub(in crate::services::discord) fn remove_tmux_session_if_current(
        &self,
        tmux_session_name: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        self.remove_tmux_session_if_current_locked(
            &guard,
            tmux_session_name,
            expected_cancel,
            None,
            None,
        )
    }
    pub(in crate::services::discord) fn cancel_and_remove_channel_if_current(
        &self,
        channel_id: &ChannelId,
        expected_tmux_session_name: &str,
        expected_output_path: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        let guard = lock_tmux_watcher_registry();
        self.cancel_and_remove_channel_if_current_locked(
            &guard,
            channel_id,
            expected_tmux_session_name,
            expected_output_path,
            expected_cancel,
            (None, None),
        )
    }

    fn remove_tmux_session_if_current_locked(
        &self,
        guard: &TmuxWatcherRegistryGuard,
        session: &str,
        cancel: &Arc<std::sync::atomic::AtomicBool>,
        identity: Option<&WatcherIdentityFence>,
        delivery: Option<&TerminalDeliveryFence>,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        self.storage.remove_tmux_session_if_current(
            Self::observers(),
            guard,
            session,
            cancel,
            identity,
            delivery,
        )
    }
    fn cancel_and_remove_channel_if_current_locked(
        &self,
        guard: &TmuxWatcherRegistryGuard,
        channel: &ChannelId,
        session: &str,
        path: &str,
        cancel: &Arc<std::sync::atomic::AtomicBool>,
        fences: (
            Option<&WatcherIdentityFence>,
            Option<&TerminalDeliveryFence>,
        ),
    ) -> bool {
        self.storage.cancel_and_remove_channel_if_current(
            guard,
            channel,
            session,
            path,
            cancel,
            (Self::observers(), fences.0, fences.1),
        )
    }
    pub(in crate::services::discord) fn restore_owner_channel_for_tmux_session(
        &self,
        session: &str,
        channel: ChannelId,
    ) -> bool {
        let guard = lock_tmux_watcher_registry();
        self.storage.restore_owner_channel_for_tmux_session(
            Self::observers(),
            &guard,
            session,
            channel,
        )
    }
    #[cfg(unix)]
    pub(in crate::services::discord) fn publish_restored_owner(
        &self,
        session: &str,
        channel: ChannelId,
        publish_source: impl FnOnce() -> bool,
    ) -> bool {
        let guard = match TMUX_WATCHER_REGISTRY_LOCK.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        let owner = self.storage.owner_channel_for_tmux_session(session);
        if owner.is_some_and(|owner| owner != channel) || !publish_source() {
            return false;
        }
        if owner.is_none() {
            self.storage.restore_owner_channel_for_tmux_session(
                Self::observers(),
                &guard,
                session,
                channel,
            );
        }
        true
    }
    pub(in crate::services::discord) fn clear_restored_owner_for_tmux_session(
        &self,
        session: &str,
    ) {
        let guard = lock_tmux_watcher_registry();
        self.clear_restored_owner_locked(&guard, session);
    }
    pub(in crate::services::discord) fn clear_restored_owner_locked(
        &self,
        guard: &TmuxWatcherRegistryGuard,
        session: &str,
    ) {
        self.storage
            .clear_restored_owner_locked(Self::observers(), guard, session);
    }
    pub(in crate::services::discord) fn retain_owner_during_session_rebind(
        &self,
        session: &str,
        channel: ChannelId,
    ) -> bool {
        let guard = lock_tmux_watcher_registry();
        self.storage
            .retain_owner_during_session_rebind(Self::observers(), &guard, session, channel)
    }
    #[cfg(test)]
    pub(in crate::services::discord) fn replace_paired_cancel_for_test(
        &self,
        session: &str,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let guard = lock_tmux_watcher_registry();
        self.storage.replace_paired_cancel_for_test(session, cancel);
        drop(guard);
    }
    #[cfg(test)]
    pub(in crate::services::discord) fn remove_pair_without_payload_for_test(
        &self,
        session: &str,
    ) -> Option<ReservationRecord> {
        let guard = lock_tmux_watcher_registry();
        self.storage
            .remove_pair_without_payload_for_test(Self::observers(), &guard, session)
    }
    #[cfg(test)]
    pub(in crate::services::discord) fn clear_reservation_for_test(
        &self,
        record: ReservationRecord,
    ) -> bool {
        let guard = lock_tmux_watcher_registry();
        self.storage.clear_reservation_for_test(&guard, record)
    }
}
// #5808 C1: the pinned watcher incarnation a turn captured at claim time.
// Moved here from `watchers/lifecycle/claims.rs` because the `tmux` module
// tree is `#[cfg(unix)]` while `turn_bridge` threads this pin on every
// target; the type itself only touches registry handles, so it is
// platform-independent. The unix-only capture path stays gated below.
#[derive(Debug, Clone)]
pub(in crate::services::discord) struct WatcherClaimIncarnation {
    owner_channel_id: ChannelId,
    // Was module-private while this type lived in `watchers/lifecycle/claims.rs`;
    // that module still reads it for the reservation-identity invariant, so the
    // field carries the same discord-scoped visibility as its siblings here.
    pub(in crate::services::discord) cancel: Arc<std::sync::atomic::AtomicBool>,
    pub(in crate::services::discord) paused: Arc<std::sync::atomic::AtomicBool>,
    pub(in crate::services::discord) resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pub(in crate::services::discord) turn_delivered: Arc<std::sync::atomic::AtomicBool>,
}

impl WatcherClaimIncarnation {
    pub(in crate::services::discord) fn from_handle(
        owner_channel_id: ChannelId,
        handle: &TmuxWatcherHandle,
    ) -> Self {
        Self {
            owner_channel_id,
            cancel: Arc::clone(&handle.cancel),
            paused: Arc::clone(&handle.paused),
            resume_offset: Arc::clone(&handle.resume_offset),
            turn_delivered: Arc::clone(&handle.turn_delivered),
        }
    }

    #[cfg(unix)]
    pub(in crate::services::discord) fn capture_for_source(
        watchers: &TmuxWatcherRegistry,
        tmux_session_name: &str,
        output_path: &std::path::Path,
    ) -> Option<Self> {
        let _guard = lock_tmux_watcher_registry();
        let owner = watchers.owner_channel_for_tmux_session(tmux_session_name)?;
        let handle = watchers.get(&owner)?;
        if handle.tmux_session_name != tmux_session_name
            || std::path::Path::new(&handle.output_path) != output_path
            || handle.cancel.load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        Some(Self::from_handle(owner, &handle))
    }

    #[rustfmt::skip]
    pub(in crate::services::discord) fn adopt_if_current<T>(
        &self,
        watchers: &TmuxWatcherRegistry,
        adopt: impl FnOnce(&Self) -> T,
    ) -> Option<T> {
        let _guard = lock_tmux_watcher_registry();
        #[cfg(test)]
        if EVICT_CLAIM_BEFORE_ADOPTION.compare_exchange(
            self.owner_channel_id.get(), 0,
            std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst,
        ).is_ok() {
            let _ = watchers.remove_locked(&_guard, &self.owner_channel_id);
        }
        let current = watchers.get(&self.owner_channel_id)?;
        if !Arc::ptr_eq(&current.cancel, &self.cancel)
            || current.cancel.load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        drop(current);
        Some(adopt(self))
    }
}

#[cfg(test)]
#[rustfmt::skip]
static EVICT_CLAIM_BEFORE_ADOPTION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(in crate::services::discord) struct ClaimAdoptionEvictionGuard(u64);

#[cfg(test)]
#[rustfmt::skip]
impl Drop for ClaimAdoptionEvictionGuard {
    fn drop(&mut self) {
        let _ = EVICT_CLAIM_BEFORE_ADOPTION.compare_exchange(
            self.0, 0, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst,
        );
    }
}

#[cfg(test)]
#[rustfmt::skip]
pub(in crate::services::discord) fn evict_claim_before_adoption_for_test(owner: ChannelId) -> ClaimAdoptionEvictionGuard {
    EVICT_CLAIM_BEFORE_ADOPTION.store(owner.get(), std::sync::atomic::Ordering::SeqCst);
    ClaimAdoptionEvictionGuard(owner.get())
}
