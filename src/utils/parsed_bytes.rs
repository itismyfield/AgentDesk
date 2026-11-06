//! A parsed file read is reused only while the file still holds the very same bytes, so an
//! unchanged file is parsed once and any rewrite, by any writer or process, parses again.
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

struct Kept {
    raw: Vec<u8>,
    value: Arc<dyn Any + Send + Sync>,
    used: u64,
}

/// Production keeps a few dozen files (history, binding logs, launch scripts); tests read many
/// temporary ones, so a full map drops its least recently used file.
const KEPT_LIMIT: usize = 128;
static TICK: AtomicU64 = AtomicU64::new(0);

static KEPT: LazyLock<Mutex<HashMap<(PathBuf, TypeId), Kept>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
thread_local! {
    /// Builds run on this thread, for the parse-count tests.
    pub(crate) static BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `build`'s value for `raw`, the bytes just read from `path`; a failed build is never kept.
pub(crate) fn reuse<T: Any + Send + Sync>(
    path: &Path,
    raw: Vec<u8>,
    build: impl FnOnce(&[u8]) -> Result<T, String>,
) -> Result<Arc<T>, String> {
    let key = (path.to_path_buf(), TypeId::of::<T>());
    let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(same) = kept.get_mut(&key).filter(|kept| kept.raw == raw) {
        same.used = TICK.fetch_add(1, Ordering::Relaxed);
        if let Ok(value) = Arc::clone(&same.value).downcast::<T>() {
            return Ok(value);
        }
    }
    drop(kept);
    #[cfg(test)]
    BUILDS.with(|builds| builds.set(builds.get() + 1));
    let value = Arc::new(build(&raw)?);
    let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    if kept.len() >= KEPT_LIMIT && !kept.contains_key(&key) {
        let oldest = kept.iter().min_by_key(|(_, kept)| kept.used);
        if let Some(oldest) = oldest.map(|(key, _)| key.clone()) {
            kept.remove(&oldest);
        }
    }
    kept.insert(
        key,
        Kept {
            raw,
            value: Arc::clone(&value) as Arc<dyn Any + Send + Sync>,
            used: TICK.fetch_add(1, Ordering::Relaxed),
        },
    );
    Ok(value)
}

/// Forgets every value kept for `path`, as a restart would.
#[cfg(test)]
pub(crate) fn forget(path: &Path) {
    let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    kept.retain(|(kept_path, _), _| kept_path != path);
}
