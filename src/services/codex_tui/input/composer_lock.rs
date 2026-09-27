use std::sync::{Arc, LazyLock, Mutex};

static CODEX_COMPOSER_MUTATION_LOCKS: LazyLock<dashmap::DashMap<String, Arc<Mutex<()>>>> =
    LazyLock::new(dashmap::DashMap::new);

pub(super) fn with_composer_mutation_lock<R>(
    session_name: &str,
    operation: impl FnOnce() -> R,
) -> R {
    let composer_lock = CODEX_COMPOSER_MUTATION_LOCKS
        .entry(session_name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _composer_guard = composer_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    operation()
}
/// Try the existing composer fence; contention or poison leaves the callback unrun.
#[allow(dead_code)]
pub(crate) fn try_with_composer_mutation_lock<R>(
    session_name: &str,
    operation: impl FnOnce() -> R,
) -> Option<R> {
    let composer_lock = CODEX_COMPOSER_MUTATION_LOCKS
        .try_entry(session_name.to_string())?
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _composer_guard = composer_lock.try_lock().ok()?;
    Some(operation())
}
