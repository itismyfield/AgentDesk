//! Test-only injected host observations keyed by host kind and session name.
//! Real probes are time-bounded and flip under load, so a test states the answer.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use super::model::{HostKind, HostLiveness, HostSessionRef};

type Injected = HashMap<(HostKind, String), HostLiveness>;

static INJECTED_LIVENESS: LazyLock<Mutex<Injected>> = LazyLock::new(Mutex::default);

fn injected() -> MutexGuard<'static, Injected> {
    INJECTED_LIVENESS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Force (or clear, with `None`) the liveness answer for one hosted session.
pub(crate) fn inject_liveness(session: HostSessionRef<'_>, liveness: Option<HostLiveness>) {
    let key = (session.kind, session.name.to_string());
    match liveness {
        Some(value) => injected().insert(key, value),
        None => injected().remove(&key),
    };
}

/// The injected answer for exactly this host kind and name.
pub(crate) fn injected_liveness(session: HostSessionRef<'_>) -> Option<HostLiveness> {
    injected()
        .get(&(session.kind, session.name.to_string()))
        .copied()
}

/// RAII form of [`inject_liveness`]; clears on drop, including on unwind.
#[must_use = "the injected answer is cleared when this guard is dropped"]
pub(crate) struct InjectedLivenessGuard {
    kind: HostKind,
    name: String,
}

impl InjectedLivenessGuard {
    pub(crate) fn set(session: HostSessionRef<'_>, liveness: HostLiveness) -> Self {
        inject_liveness(session, Some(liveness));
        Self {
            kind: session.kind,
            name: session.name.to_string(),
        }
    }
}

impl Drop for InjectedLivenessGuard {
    fn drop(&mut self) {
        let session = HostSessionRef {
            kind: self.kind,
            name: &self.name,
        };
        inject_liveness(session, None);
    }
}
