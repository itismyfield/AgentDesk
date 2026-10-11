//! Dormant observation runtime for Codex history adoption. Only a Codex boot with
//! `tui_o.codex_history_adoption` on installs one; otherwise producers find none and record nothing.

pub(super) mod boot;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use super::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::shadow::tap::TuiOConfig;

/// The observation of one provider boot. It holds its SharedData weakly, so it ends with that
/// runtime and never keeps a restarted one alive.
pub(super) struct AdoptionRuntime {
    /// Process-unique; a witness names it, so another boot's evidence is never this one's.
    id: u64,
    provider: ProviderKind,
    bot: String,
    shared: Weak<SharedData>,
    /// The configured writer channels the boot read is logged for.
    targets: BTreeSet<u64>,
    boot: boot::BootState,
}

/// Names the runtime that observed a piece of evidence, so a later runtime never reuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ObservationRuntimeId(u64);

impl AdoptionRuntime {
    pub(super) fn observation_runtime_id(&self) -> ObservationRuntimeId {
        ObservationRuntimeId(self.id)
    }
}

#[cfg(test)]
impl ObservationRuntimeId {
    pub(super) fn for_tests(id: u64) -> Self {
        Self(id)
    }
}

static RUNTIMES: Mutex<Vec<Arc<AdoptionRuntime>>> = Mutex::new(Vec::new());
static NEXT_RUNTIME: AtomicU64 = AtomicU64::new(1);

fn runtimes() -> MutexGuard<'static, Vec<Arc<AdoptionRuntime>>> {
    RUNTIMES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Installs the runtime for `shared` when the boot is Codex's and the flag is on; else `None`.
pub(super) fn install(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    bot: &str,
    config: Option<&TuiOConfig>,
) -> Option<Arc<AdoptionRuntime>> {
    let config = config.filter(|config| config.codex_history_adoption)?;
    if *provider != ProviderKind::Codex {
        return None;
    }
    let runtime = Arc::new(AdoptionRuntime {
        id: NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed),
        provider: provider.clone(),
        bot: bot.to_owned(),
        shared: Arc::downgrade(shared),
        targets: config.writer.channels.clone(),
        boot: boot::BootState::default(),
    });
    let mut installed = runtimes();
    // An ended runtime leaves here; until then its Weak keeps the SharedData address unreused.
    installed.retain(|entry| entry.shared.strong_count() > 0);
    installed.push(runtime.clone());
    Some(runtime)
}

/// The live runtime installed for this very SharedData, if any.
pub(super) fn installed(shared: &Arc<SharedData>) -> Option<Arc<AdoptionRuntime>> {
    runtimes()
        .iter()
        .find(|entry| {
            entry.shared.as_ptr() == Arc::as_ptr(shared) && entry.shared.strong_count() > 0
        })
        .cloned()
}
