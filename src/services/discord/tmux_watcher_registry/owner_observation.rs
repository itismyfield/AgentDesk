//! A process-wide invalidation epoch and synchronous owner publication fence.
use std::sync::atomic::{AtomicU64, Ordering};

use super::{TMUX_WATCHER_REGISTRY_LOCK, TmuxWatcherRegistryGuard};

static OWNER_EPOCH: AtomicU64 = AtomicU64::new(0);

pub(super) fn bump_owner_observation_epoch(_guard: &TmuxWatcherRegistryGuard) {
    // Saturation permanently fails closed instead of reusing an old observation identity.
    let _ = OWNER_EPOCH.fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
        Some(epoch.saturating_add(1))
    });
}

pub(in crate::services::discord) fn owner_observation_epoch() -> Option<u64> {
    let epoch = OWNER_EPOCH.load(Ordering::Acquire);
    (epoch != u64::MAX).then_some(epoch)
}

pub(in crate::services::discord) fn with_owner_publication<R>(
    expected_epoch: u64,
    publish: impl FnOnce(&TmuxWatcherRegistryGuard) -> R,
) -> Result<R, &'static str> {
    let guard = TMUX_WATCHER_REGISTRY_LOCK
        .try_lock()
        .map_err(|_| "registry_busy")?;
    if owner_observation_epoch() != Some(expected_epoch) {
        return Err("owner_epoch");
    }
    #[cfg(test)]
    let _scope = PublicationScope::enter();
    Ok(publish(&guard))
}

#[cfg(test)]
thread_local! {
    static PUBLICATION_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
struct PublicationScope(bool);

#[cfg(test)]
impl PublicationScope {
    fn enter() -> Self {
        Self(PUBLICATION_HELD.replace(true))
    }
}

#[cfg(test)]
impl Drop for PublicationScope {
    fn drop(&mut self) {
        PUBLICATION_HELD.set(self.0);
    }
}

#[cfg(test)]
pub(crate) fn owner_publication_fence_held_for_tests() -> bool {
    PUBLICATION_HELD.get()
}
