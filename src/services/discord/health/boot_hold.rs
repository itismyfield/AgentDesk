//! Provider runtimes held at boot, before they have a health registry entry to probe.
//! The snapshot reports their reasons like other process-level boot reasons.
use std::collections::BTreeMap;

#[cfg(not(test))]
static HOLDS: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, String>>> =
    std::sync::LazyLock::new(Default::default);

// Each tokio test owns its thread, so a test's holds never reach another test's snapshot.
#[cfg(test)]
thread_local! {
    static HOLDS: std::cell::RefCell<BTreeMap<String, String>> = Default::default();
}

#[cfg(not(test))]
fn with_holds<T>(f: impl FnOnce(&mut BTreeMap<String, String>) -> T) -> T {
    f(&mut HOLDS.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
fn with_holds<T>(f: impl FnOnce(&mut BTreeMap<String, String>) -> T) -> T {
    HOLDS.with(|holds| f(&mut holds.borrow_mut()))
}

/// Records why `provider`'s runtime is not running yet; the latest reason wins.
#[cfg(unix)]
pub(in crate::services::discord) fn hold(provider: &str, kind: &str, reason: &str) {
    let reason = format!("{kind}:{}", reason.replace(' ', "_"));
    with_holds(|holds| holds.insert(provider.to_owned(), reason));
}

/// The runtime got past its boot gate.
#[cfg(unix)]
pub(in crate::services::discord) fn release(provider: &str) {
    with_holds(|holds| holds.remove(provider));
}

/// `provider:<name>:<kind>:<reason>` for every held runtime.
pub(in crate::services::discord) fn health_reasons() -> Vec<String> {
    with_holds(|holds| {
        holds
            .iter()
            .map(|(provider, reason)| format!("provider:{provider}:{reason}"))
            .collect()
    })
}
