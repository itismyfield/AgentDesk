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
pub(crate) fn invalidate_shared_knowledge_cache_for_tests() {
    let mut guard = shared_knowledge_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    fn with_temp_root<F>(f: F)
    where
        F: FnOnce(&std::path::Path),
    {
        let _guard = super::super::runtime_store::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join(".adk");
        let sak_dir = root
            .join("config")
            .join("memories")
            .join("shared-agent-knowledge");
        std::fs::create_dir_all(&sak_dir).unwrap();
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        f(&sak_dir);
        match prev {
            Some(v) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", v) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[test]
    fn test_load_shared_knowledge_empty_returns_none() {
        with_temp_root(|sam| {
            std::fs::write(sam.join("shared_knowledge.md"), "   ").unwrap();
            assert!(load_shared_knowledge().is_none());
        });
    }

    #[test]
    fn test_load_shared_knowledge_returns_wrapped() {
        with_temp_root(|sam| {
            std::fs::write(sam.join("shared_knowledge.md"), "Some knowledge").unwrap();
            invalidate_shared_knowledge_cache_for_tests();
            let result = load_shared_knowledge().unwrap();
            assert_eq!(result, "[Shared Agent Knowledge]\nSome knowledge");
        });
    }

    /// #2663: cached rendering must be reused while the source file is
    /// unchanged, then re-rendered when the file mtime advances past the
    /// cached value.
    #[test]
    fn test_load_shared_knowledge_cache_reuses_and_invalidates() {
        with_temp_root(|sam| {
            let path = sam.join("shared_knowledge.md");
            std::fs::write(&path, "Initial knowledge").unwrap();
            invalidate_shared_knowledge_cache_for_tests();

            let first = load_shared_knowledge().unwrap();
            assert!(first.contains("Initial knowledge"));
            let second = load_shared_knowledge().unwrap();
            assert_eq!(first, second, "cache hit must produce identical output");

            // Wait past the FS mtime resolution and rewrite. The next call
            // should see the new content.
            std::thread::sleep(std::time::Duration::from_millis(1100));
            std::fs::write(&path, "Updated knowledge").unwrap();
            let third = load_shared_knowledge().unwrap();
            assert!(
                third.contains("Updated knowledge"),
                "cache must invalidate on mtime change: got {third}"
            );
        });
    }

    /// #2663: missing file returns `None` and stays in the negative cache;
    /// when the file appears later the cache picks it up.
    #[test]
    fn test_load_shared_knowledge_cache_handles_missing_file() {
        with_temp_root(|sam| {
            invalidate_shared_knowledge_cache_for_tests();
            // No file present yet → None.
            assert!(load_shared_knowledge().is_none());
            // Now create the file and verify the cache resolves to a hit.
            std::fs::write(sam.join("shared_knowledge.md"), "Appeared").unwrap();
            // Sleep past mtime resolution so the fingerprint shifts away from
            // the "missing" entry.
            std::thread::sleep(std::time::Duration::from_millis(1100));
            let rendered = load_shared_knowledge().expect("cache must pick up newly created file");
            assert!(rendered.contains("Appeared"));
        });
    }
}
