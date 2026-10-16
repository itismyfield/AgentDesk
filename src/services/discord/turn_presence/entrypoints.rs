//! Presence withdrawal hooks for service entry points; each returns at once while no runtime is
//! installed, and none of them creates a registration, a send or a stored fact.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

/// What an installed presence runtime lets an entry point revoke; it holds no Busy or turn facts.
pub(in crate::services::discord) trait Fence: Send + Sync {
    /// Revokes the channel's existing registration without creating or reviving one.
    fn withdraw(&self, channel: u64, retire: bool, cause: &'static str);
    /// The channel's current registration generation; adoption leaves it, a reset replaces it.
    fn generation(&self, channel: u64) -> Option<Arc<()>>;
    /// Revokes the channel's registration only while it is still `generation`.
    fn withdraw_generation(&self, channel: u64, generation: &Arc<()>, cause: &'static str);
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

/// Runtimes whose owner passes `owns`; for sinks that hold no `SharedData` of their own.
fn owned_where(owns: impl Fn(&SharedData) -> bool) -> impl Iterator<Item = Arc<dyn Fence>> {
    installed()
        .into_iter()
        .filter(move |(owner, _)| owner.upgrade().is_some_and(|owner| owns(&owner)))
        .map(|(_, fence)| fence)
}

/// A real change to the channel's turn: its earlier approvals in `shared`'s runtime may not start
/// another typing.
pub(in crate::services::discord) fn withdraw(
    shared: &SharedData,
    channel: u64,
    cause: &'static str,
) {
    owned_by(shared).for_each(|fence| fence.withdraw(channel, false, cause));
}

/// The registrations a holder saw when it started; a later reset or retire is not its to undo.
pub(crate) struct Lease {
    channel: u64,
    held: Vec<(Weak<dyn Fence>, Arc<()>)>,
}

impl Lease {
    pub(crate) fn take(provider: &ProviderKind, channel: u64) -> Self {
        let held = owned_where(|shared| shared.provider == *provider)
            .filter_map(|fence| Some((Arc::downgrade(&fence), fence.generation(channel)?)))
            .collect();
        Self { channel, held }
    }

    pub(crate) fn withdraw(&self, cause: &'static str) {
        for (fence, generation) in &self.held {
            if let Some(fence) = fence.upgrade() {
                fence.withdraw_generation(self.channel, generation, cause);
            }
        }
    }
}
