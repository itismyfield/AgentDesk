//! A history parse is reused without reading while the file keeps its identity, size and times,
//! its last write is older than any clock tick, and no writer in this process has published since.
use crate::services::tui_o::shadow::capture::Fingerprint;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

/// A write after the stamp was taken lands at least this far past a settled mtime, so a coarse
/// clock cannot give a rewritten file the same times.
const SETTLED: Duration = Duration::from_secs(2);
const KEPT_LIMIT: usize = 32;
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct Kept {
    stamp: Fingerprint,
    generation: u64,
    value: Arc<dyn Any + Send + Sync>,
}

static KEPT: LazyLock<Mutex<HashMap<(PathBuf, TypeId), Kept>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
thread_local! {
    /// History byte reads on this thread, for the cost tests.
    pub(crate) static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The history file state a kept answer was computed under; equal only while no write landed.
#[derive(Clone, PartialEq)]
pub(crate) struct Token {
    path: PathBuf,
    stamp: Fingerprint,
    generation: u64,
}

/// The file's current state, or `None` while it is missing, freshly written or carries no
/// identity and change time to tell a same-length rewrite apart.
pub(super) fn token(path: &Path) -> Option<Token> {
    let generation = GENERATION.load(Ordering::SeqCst);
    let now = SystemTime::now();
    let stamp = Fingerprint::of(&std::fs::metadata(path).ok()?).ok()?;
    settled(&stamp, now).then(|| Token {
        path: path.to_path_buf(),
        stamp,
        generation,
    })
}

/// Only Unix stamps carry the inode and change time that tell a rewrite with a restored mtime
/// apart, so other platforms always read.
fn settled(stamp: &Fingerprint, now: SystemTime) -> bool {
    cfg!(unix)
        && stamp
            .modified
            .checked_add(SETTLED)
            .is_some_and(|at| at <= now)
}

/// Called after every in-process history write, published or not.
pub(super) fn published() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// `build`'s value for the file's current bytes; open and read failures stay io errors.
pub(super) fn read<T: Any + Send + Sync>(
    path: &Path,
    build: impl FnOnce(&[u8]) -> Result<T, String>,
) -> std::io::Result<Result<Arc<T>, String>> {
    let generation = GENERATION.load(Ordering::SeqCst);
    let now = SystemTime::now();
    let mut file = std::fs::File::open(path)?;
    let stamp = Fingerprint::of(&file.metadata()?)?;
    let settled = settled(&stamp, now);
    let key = (path.to_path_buf(), TypeId::of::<T>());
    let kept = || KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    if settled
        && let Some(value) = kept()
            .get(&key)
            .filter(|kept| kept.stamp == stamp && kept.generation == generation)
            .and_then(|kept| Arc::clone(&kept.value).downcast::<T>().ok())
    {
        return Ok(Ok(value));
    }
    #[cfg(test)]
    READS.with(|reads| reads.set(reads.get() + 1));
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)?;
    let value = match super::compat::reuse(path, raw, build) {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    if settled {
        let mut kept = kept();
        if kept.len() >= KEPT_LIMIT && !kept.contains_key(&key) {
            kept.clear();
        }
        let value = Arc::clone(&value) as Arc<dyn Any + Send + Sync>;
        kept.insert(
            key,
            Kept {
                stamp,
                generation,
                value,
            },
        );
    }
    Ok(Ok(value))
}
