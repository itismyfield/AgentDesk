use super::LogRead;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

struct Cached {
    stamp: Option<(u64, SystemTime)>,
    read: Box<dyn Any + Send>,
}

type ParsedReads = HashMap<TypeId, Cached>;
static READS: LazyLock<Mutex<HashMap<PathBuf, ParsedReads>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn stamp(path: &Path) -> io::Result<Option<(u64, SystemTime)>> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some((meta.len(), meta.modified()?))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

// Callers hold LOGS; this cache never acquires it. Parser types keep their own validity rules.
pub(super) fn read<T: Clone + Send + 'static>(
    path: &Path,
    load: impl FnOnce() -> io::Result<LogRead<T>>,
) -> io::Result<LogRead<T>> {
    let before = stamp(path)?;
    let kind = TypeId::of::<T>();
    let mut reads = READS.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(read) = reads
        .get(path)
        .and_then(|parsed| parsed.get(&kind))
        .filter(|cached| cached.stamp == before)
        .and_then(|cached| cached.read.downcast_ref::<LogRead<T>>())
    {
        return Ok(read.clone());
    }
    let read = load()?;
    if stamp(path)? == before {
        reads.entry(path.to_path_buf()).or_default().insert(
            kind,
            Cached {
                stamp: before,
                read: Box::new(read.clone()),
            },
        );
    }
    Ok(read)
}

// Truncate/append/rollback may restore both length and a coarse timestamp, so invalidate first.
pub(super) fn invalidate(path: &Path) {
    #[cfg(test)]
    if super::n2b_mutant("binding-log-invalidate") {
        return;
    }
    READS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(path);
}
