use super::*;
use std::sync::Arc;

/// A pause point around the owning decision.
type Probe = Arc<dyn Fn(&str) + Send + Sync>;
static BEFORE_COMMIT: Mutex<Option<Probe>> = Mutex::new(None);
static BEFORE_PARK: Mutex<Option<Probe>> = Mutex::new(None);
static BEFORE_PUBLISH: Mutex<Option<Probe>> = Mutex::new(None);

fn installed(slot: &Mutex<Option<Probe>>) -> Option<Probe> {
    slot.lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// Runs under the STATE lock, after the duplicate checks and before the owning commit.
pub(crate) fn before_owning_commit(prompt: &str) {
    if let Some(probe) = installed(&BEFORE_COMMIT) {
        probe(prompt);
    }
}

/// Runs under the STATE lock before an observation can join the parked inputs.
pub(crate) fn before_parking_commit(prompt: &str) {
    if let Some(probe) = installed(&BEFORE_PARK) {
        probe(prompt);
    }
}

/// Runs after the decision released the lock and before the owning event is published.
pub(crate) fn before_owning_publish(prompt: &str) {
    if let Some(probe) = installed(&BEFORE_PUBLISH) {
        probe(prompt);
    }
}

pub struct ProbeGuard(&'static Mutex<Option<Probe>>);

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }
}

fn install(slot: &'static Mutex<Option<Probe>>, probe: Probe) -> ProbeGuard {
    *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(probe);
    ProbeGuard(slot)
}

#[cfg(test)]
pub(crate) fn on_owning_commit(probe: impl Fn(&str) + Send + Sync + 'static) -> ProbeGuard {
    install(&BEFORE_COMMIT, Arc::new(probe))
}

pub fn on_owning_publish(probe: impl Fn(&str) + Send + Sync + 'static) -> ProbeGuard {
    install(&BEFORE_PUBLISH, Arc::new(probe))
}
