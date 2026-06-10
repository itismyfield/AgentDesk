use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use super::runtime_store::shared_agent_knowledge_path;

/// #2663: process-local cache for the rendered SAK section. The Shared Agent
/// Knowledge file is small (~2-3KB) but is re-prepended to every Codex/Claude
/// turn, so the per-turn file read and per-turn format!() allocation show up
/// in steady-state CPU profiles. Caching keyed on (path, mtime) lets us hit
/// the fast path while still picking up out-of-band edits the operator makes
/// to `shared_knowledge.md`.
#[derive(Clone)]
struct SharedKnowledgeCacheEntry {
    path: PathBuf,
    mtime: Option<SystemTime>,
    /// `None` means "we read the file and it was empty/missing"; the call site
    /// still benefits because we avoid the `fs::read_to_string` allocation on
    /// repeated turns.
    rendered: Option<String>,
}

fn shared_knowledge_cache() -> &'static Mutex<Option<SharedKnowledgeCacheEntry>> {
    static CELL: OnceLock<Mutex<Option<SharedKnowledgeCacheEntry>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

fn file_mtime(path: &std::path::Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// Read shared_knowledge.md from the managed SAK path.
/// Returns the file content wrapped in a [Shared Agent Knowledge] section,
/// or None if the file doesn't exist or is empty.
///
/// #2663: the rendered section is cached process-locally, keyed on the file's
/// canonical path + mtime. The next call only re-reads the file when mtime
/// changes (or the path moves), turning the hot path into a hash lookup +
/// Arc-like String clone instead of a syscall + format!.
pub(crate) fn load_shared_knowledge() -> Option<String> {
    let path = shared_agent_knowledge_path()?;
    let current_mtime = file_mtime(&path);
    let mut guard = shared_knowledge_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(entry) = guard.as_ref() {
        if entry.path == path && entry.mtime == current_mtime {
            return entry.rendered.clone();
        }
    }

    let rendered = match fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!("[Shared Agent Knowledge]\n{}", trimmed))
            }
        }
        Err(_) => None,
    };
    *guard = Some(SharedKnowledgeCacheEntry {
        path,
        mtime: current_mtime,
        rendered: rendered.clone(),
    });
    rendered
}

/// #2663: test-only helper to evict the SAK cache between scenarios.
#[cfg(test)]
#[allow(dead_code)] // #3034: test cache-reset helper; no active test caller currently.
pub(crate) fn invalidate_shared_knowledge_cache_for_tests() {
    let mut guard = shared_knowledge_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}
