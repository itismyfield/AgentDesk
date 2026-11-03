//! A parsed history read is reused only while the file still holds the very same bytes, so an
//! unchanged history is parsed once and any rewrite, by any writer or process, parses again.
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

struct Kept {
    raw: Vec<u8>,
    value: Arc<dyn Any + Send + Sync>,
}

/// Production reads one history path; tests read many temporary ones, so the map stays small.
const KEPT_LIMIT: usize = 16;

static KEPT: LazyLock<Mutex<HashMap<(PathBuf, TypeId), Kept>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `build`'s value for `raw`, the bytes just read from `path`; a failed build is never kept.
pub(in super::super) fn reuse<T: Any + Send + Sync>(
    path: &Path,
    raw: Vec<u8>,
    build: impl FnOnce(&[u8]) -> Result<T, String>,
) -> Result<Arc<T>, String> {
    let key = (path.to_path_buf(), TypeId::of::<T>());
    let kept = KEPT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .filter(|kept| kept.raw == raw)
        .map(|kept| Arc::clone(&kept.value));
    if let Some(value) = kept.and_then(|value| value.downcast::<T>().ok()) {
        return Ok(value);
    }
    let value = Arc::new(build(&raw)?);
    let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    if kept.len() >= KEPT_LIMIT && !kept.contains_key(&key) {
        kept.clear();
    }
    kept.insert(
        key,
        Kept {
            raw,
            value: Arc::clone(&value) as Arc<dyn Any + Send + Sync>,
        },
    );
    Ok(value)
}
