//! Presence withdrawal hooks for service entry points; each returns at once while no runtime is
//! installed, and none of them creates a registration, a send or a stored fact.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::services::discord::SharedData;
use crate::services::tui_o::ownership::OwnershipGate;

/// What an installed presence runtime lets an entry point revoke; it holds no Busy or turn facts.
pub(in crate::services::discord) trait Fence: Send + Sync {
    /// Revokes the channel's existing registration without creating or reviving one.
    fn withdraw(&self, channel: u64, retire: bool, cause: &'static str);
    /// Revokes only approvals resting on this Gateway witness; Home-owned ones stay.
    fn withdraw_gateway(&self, gate: &Arc<OwnershipGate>, cause: &'static str);
    fn suspend(&self);
    /// Reopens with fresh registrations only; no earlier approval or deadline comes back.
    fn resume(&self);
}

type Installed = (Weak<SharedData>, Weak<dyn Fence>);

static INSTALLED_COUNT: AtomicUsize = AtomicUsize::new(0);
static INSTALLED: LazyLock<Mutex<Vec<Installed>>> = LazyLock::new(Default::default);

/// The count is read before the list, so an uninstalled process takes no lock.
fn installed() -> Vec<(Weak<SharedData>, Arc<dyn Fence>)> {
    if INSTALLED_COUNT.load(Ordering::Acquire) == 0 {
        return Vec::new();
    }
    let installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
    installed
        .iter()
        .filter_map(|(shared, fence)| Some((shared.clone(), fence.upgrade()?)))
        .collect()
}

fn owned_by(shared: &SharedData) -> impl Iterator<Item = Arc<dyn Fence>> + '_ {
    installed()
        .into_iter()
        .filter(move |(owner, _)| std::ptr::eq(owner.as_ptr(), shared))
        .map(|(_, fence)| fence)
}

/// A real change to the channel's turn: its earlier approvals may not start another typing.
pub(in crate::services::discord) fn withdraw(channel: u64, cause: &'static str) {
    for (_, fence) in installed() {
        fence.withdraw(channel, false, cause);
    }
}

/// Only for a channel whose session was actually removed; its registration is pruned.
pub(in crate::services::discord) fn retire(channel: u64) {
    for (_, fence) in installed() {
        fence.withdraw(channel, true, "session_removed");
    }
}

/// The channel whose live watcher holds `session` now; the name itself is never parsed.
pub(in crate::services::discord) fn withdraw_session(session: &str, cause: &'static str) {
    for (shared, fence) in installed() {
        let Some(shared) = shared.upgrade() else {
            continue;
        };
        let watchers = &shared.tmux_watchers;
        let owner = watchers.owner_channel_for_tmux_session(session);
        let live = owner.filter(|channel| {
            let binding = watchers.channel_binding(channel);
            binding.is_some_and(|binding| binding.tmux_session_name == session)
        });
        if let Some(channel) = live {
            fence.withdraw(channel.get(), false, cause);
        }
    }
}

pub(in crate::services::discord) fn withdraw_gateway(
    gate: &Arc<OwnershipGate>,
    cause: &'static str,
) {
    for (_, fence) in installed() {
        fence.withdraw_gateway(gate, cause);
    }
}

pub(in crate::services::discord) fn suspend(shared: &SharedData) {
    owned_by(shared).for_each(|fence| fence.suspend());
}

/// Only where the existing restart decision rolls the fence back.
pub(in crate::services::discord) fn resume_fresh(shared: &SharedData) {
    owned_by(shared).for_each(|fence| fence.resume());
}

#[cfg(all(test, unix))]
#[path = "entrypoints_tests.rs"]
pub(in crate::services::discord) mod tests;
