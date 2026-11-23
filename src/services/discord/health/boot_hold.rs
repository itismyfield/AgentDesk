//! Provider runtimes held at boot, before they register with the health registry (#8351).
//!
//! A held runtime has no `SharedData` entry to probe yet, so its reason lives here
//! and the snapshot reports it the same way it reports process-level writer alarms.
use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

static HOLDS: LazyLock<Mutex<BTreeMap<String, String>>> = LazyLock::new(Default::default);

/// Records why `provider`'s runtime is not running yet; the latest reason wins.
#[cfg(unix)]
pub(in crate::services::discord) fn hold(provider: &str, kind: &str, reason: &str) {
    HOLDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(provider.to_owned(), format!("{kind}:{}", reason.replace(' ', "_")));
}

/// The runtime got past its boot gate.
#[cfg(unix)]
pub(in crate::services::discord) fn release(provider: &str) {
    HOLDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(provider);
}

/// `provider:<name>:<kind>:<reason>` for every held runtime.
pub(in crate::services::discord) fn health_reasons() -> Vec<String> {
    HOLDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(provider, reason)| format!("provider:{provider}:{reason}"))
        .collect()
}
