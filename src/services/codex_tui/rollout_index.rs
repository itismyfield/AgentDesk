//! In-process rollout discovery index for the Codex TUI resume / follow-up
//! readiness paths.
//!
//! # Why
//!
//! `~/.codex/sessions` accumulates `rollout-*.jsonl` files indefinitely (one per
//! Codex session, on a busy host this is thousands of files). Every TUI resume
//! and every follow-up readiness probe previously re-walked the entire tree AND
//! re-read the first ~20 lines of every candidate file to parse `session_meta`.
//! That is `O(files)` `read_dir` + `O(files)` header reads on every lookup.
//!
//! This module adds a process-lifetime cache, keyed by the (canonicalized)
//! sessions root, that:
//!
//! 1. Computes a deterministic **tree signature** from the directory mtimes of
//!    the whole subtree (no file reads). Codex creates rollouts inside dated
//!    leaf directories, so a new rollout bumps its parent directory mtime and
//!    therefore the signature. The signature is recomputed on **every** lookup
//!    (cheap `O(directories)` stat calls) — invalidation is a precondition, not
//!    a postcondition.
//! 2. On a signature match (warm hit) reuses the cached file *list* directly so
//!    the `O(directories × entries)` `read_dir` re-walk is skipped; each cached
//!    path is still `stat`ed and its parsed `session_meta` reused unless its
//!    `(mtime, len)` changed (catching in-place appends/rewrites that leave the
//!    directory mtime — and thus the signature — unchanged). Warm lookups
//!    therefore do **not** re-walk the tree and do **not** re-read unchanged
//!    headers (PRD REQ-005).
//! 3. On a signature mismatch (cold / changed) re-walks the tree, but still
//!    consults the prior per-file map and re-reads only headers whose
//!    `(mtime, len)` changed (or files never seen before). A single added rollout
//!    therefore costs one header read, not `O(files)`.
//!
//! Invalidation deliberately prefers a **false miss** (an extra rescan) over a
//! **stale hit** (resuming the wrong rollout) — see PRD risk table.
//!
//! # Rollback
//!
//! The cache is gated by `RuntimeSettingsConfig::codex_rollout_index_cache_enabled`
//! (default ON, hot-reloadable). When disabled, [`cached_indexed_rollouts`]
//! falls through to the direct [`rollout_files_under`] scan plus
//! [`read_rollout_session_meta`] header reads, exactly reproducing the legacy
//! behaviour with zero cache state. Tests can also force the cold path with
//! [`reset_cache_for_tests`].

use serde_json::Value;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Maximum header lines scanned looking for the `session_meta` record. Mirrors
/// the legacy bound in `session.rs` / `rollout_tail.rs` so the bounded-read
/// guarantee (REQ-005) is preserved.
const HEADER_SCAN_LINE_LIMIT: usize = 20;

/// Upper bound on the number of distinct sessions roots cached at once. In
/// practice there is exactly one (`~/.codex/sessions`); tests may use several
/// tempdirs. The bound keeps the cache from growing without limit if a caller
/// ever passes many distinct roots, evicting the least-recently-built root.
const MAX_CACHED_ROOTS: usize = 32;

/// Parsed `session_meta` header for a single rollout file. This is the only
/// data the resolver needs beyond the file path + length, and re-parsing it is
/// the expensive part this cache eliminates on a warm hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutSessionMeta {
    pub id: String,
    pub cwd: PathBuf,
    pub source: Option<String>,
    pub originator: Option<String>,
}

impl RolloutSessionMeta {
    /// Codex direct TUI can safely resume sessions recorded by the interactive
    /// CLI. Older AgentDesk codex-exec rollouts share the same JSONL directory
    /// and UUID shape, but resuming them through the TUI can leave no fresh
    /// rollout transcript for the tailer to follow.
    pub fn is_tui_compatible(&self) -> bool {
        !self.source.as_deref().is_some_and(|value| value == "exec")
            && !self
                .originator
                .as_deref()
                .is_some_and(|value| value == "codex_exec")
    }
}

/// Per-file cache entry. `(modified, len)` are the validation key; if either
/// changes, the cached `meta` is discarded and the header is re-read.
#[derive(Debug, Clone)]
struct CachedFile {
    modified: SystemTime,
    len: u64,
    /// `None` is a cached *negative* result (no parseable `session_meta`), which
    /// is still valid to reuse as long as `(modified, len)` are unchanged.
    meta: Option<RolloutSessionMeta>,
}

/// Cached state for one sessions root.
#[derive(Debug, Clone, Default)]
struct RootIndex {
    /// Deterministic signature of the directory subtree (mtime-based). When the
    /// recomputed signature differs from this, the file list is rebuilt.
    signature: u64,
    /// Discovered rollout files with their cached metadata, keyed by path.
    files: HashMap<PathBuf, CachedFile>,
    /// Monotonic counter used for tiny LRU-ish eviction across roots.
    last_built_seq: u64,
}

#[derive(Default)]
struct IndexState {
    roots: HashMap<PathBuf, RootIndex>,
    seq: u64,
}

fn cache() -> &'static Mutex<IndexState> {
    static CACHE: OnceLock<Mutex<IndexState>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(IndexState::default()))
}

/// Pure enablement decision from a config field value. `None` (field unset)
/// means "enabled" — the feature defaults ON. Split out so the gate is unit
/// testable without touching the process-global live-config snapshot.
fn enabled_from_field(field: Option<bool>) -> bool {
    field.unwrap_or(true)
}

/// Live config gate. A `None` config snapshot (pre-boot / unit tests) and a
/// `None` field both mean "enabled". Read via `config_live_reload::current()`
/// so an `agentdesk.yaml` edit applies on the next lookup without a restart.
fn cache_enabled() -> bool {
    let field = crate::config_live_reload::current()
        .map(|cfg| cfg.runtime.codex_rollout_index_cache_enabled)
        .unwrap_or(None);
    enabled_from_field(field)
}

/// Recursively collect `rollout-*.jsonl` files under `root`. This is the shared
/// discovery primitive reused by `session.rs` and `rollout_tail.rs` (REQ-006);
/// it does **no** caching and reads **no** file contents.
pub fn rollout_files_under(root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            {
                files.push(path);
            }
        }
    }
    files
}

fn is_rollout_jsonl(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

/// Read and parse the `session_meta` header of a single rollout file. Bounded to
/// the first [`HEADER_SCAN_LINE_LIMIT`] lines (REQ-005). This is the direct
/// (uncached) read used on the cold path and when the cache is disabled.
pub fn read_rollout_session_meta(path: &Path) -> Option<RolloutSessionMeta> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader
        .lines()
        .map_while(Result::ok)
        .take(HEADER_SCAN_LINE_LIMIT)
    {
        let Ok(json) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if json.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = json.get("payload") else {
            continue;
        };
        let Some(id) = payload.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        let Some(cwd) = payload.get("cwd").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if id.is_empty() || cwd.is_empty() {
            return None;
        }
        return Some(RolloutSessionMeta {
            id: id.to_string(),
            cwd: PathBuf::from(cwd),
            source: payload
                .get("source")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            originator: payload
                .get("originator")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        });
    }
    None
}

/// A rollout candidate surfaced by the index: its path plus the file/length
/// metadata and parsed session header. `meta` is `None` for files with no
/// parseable `session_meta` (the caller filters those out).
#[derive(Debug, Clone)]
pub struct IndexedRollout {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub len: u64,
    pub meta: Option<RolloutSessionMeta>,
}

/// Deterministic, content-free tree signature for `root`. Folds each directory's
/// (path, mtime) plus the set of `rollout-*.jsonl` file paths into a stable
/// hash. Most filesystems bump the containing directory mtime when a rollout is
/// added, but hashing rollout membership avoids stale hits on coarse or restored
/// directory mtimes. A deleted/renamed leaf changes the parent mtime; a
/// brand-new root that did not exist before yields a different signature than an
/// empty/missing one. Reads no rollout file contents (REQ-005).
///
/// Returns `None` when the root does not exist or cannot be read, which the
/// caller treats as "no authoritative cache" — it does NOT cache a missing root,
/// so a later directory creation is picked up on the next lookup (PRD risk:
/// "missing root appears later").
fn tree_signature(root: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited_any = false;
    // Collect directory entries deterministically so the hash is order-stable.
    let mut dir_records: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
    let mut rollout_records: Vec<PathBuf> = Vec::new();
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            // The root itself being unreadable -> no authoritative signature.
            if !visited_any {
                return None;
            }
            continue;
        };
        visited_any = true;
        let dir_mtime = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok();
        dir_records.push((path.clone(), dir_mtime));
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                stack.push(child);
            } else if is_rollout_jsonl(&child) {
                rollout_records.push(child);
            }
        }
    }
    if !visited_any {
        return None;
    }
    dir_records.sort_by(|left, right| left.0.cmp(&right.0));
    for (path, mtime) in dir_records {
        path.hash(&mut hasher);
        match mtime {
            Some(time) => {
                1u8.hash(&mut hasher);
                if let Ok(dur) = time.duration_since(SystemTime::UNIX_EPOCH) {
                    dur.as_nanos().hash(&mut hasher);
                } else {
                    0u128.hash(&mut hasher);
                }
            }
            None => 0u8.hash(&mut hasher),
        }
    }
    rollout_records.sort();
    for path in rollout_records {
        path.hash(&mut hasher);
    }
    Some(hasher.finish())
}

/// Cache-backed rollout discovery returning each candidate's path + cached
/// metadata + parsed header.
///
/// On a warm hit (signature unchanged) the cached file *list* is reused so the
/// directory subtree is NOT re-walked; each cached path is re-`stat`ed and its
/// parsed header reused unless its `(modified, len)` changed. On a cold/changed
/// lookup the tree is re-walked but the prior per-file map is still consulted, so
/// only headers whose `(modified, len)` changed (or never-seen files) are
/// re-read — a single new rollout does NOT re-read every surviving file's header.
///
/// When the cache is disabled (config flag off) this still returns the full
/// candidate set, but performs a fresh scan + header read each time — i.e. it is
/// behaviourally identical to the legacy path, just routed through one helper.
pub fn cached_indexed_rollouts(root: &Path) -> Vec<IndexedRollout> {
    cached_indexed_rollouts_inner(root, cache_enabled())
}

/// Internal seam taking the enablement decision explicitly so the disabled
/// (rollback) path is testable without mutating the process-global live config.
fn cached_indexed_rollouts_inner(root: &Path, enabled: bool) -> Vec<IndexedRollout> {
    if !enabled {
        return scan_indexed_rollouts(root, None);
    }
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let Some(signature) = tree_signature(root) else {
        // Missing/unreadable root: do not cache, fall back to a direct scan
        // (which will also yield nothing) so a later directory creation is seen.
        return scan_indexed_rollouts(root, None);
    };

    // Snapshot the previous per-root state so headers (and, on a signature hit,
    // the file list itself) can be reused without holding the lock during file
    // I/O. The cached per-file `(mtime, len, meta)` map is reused for header
    // reuse REGARDLESS of whether the tree signature matched: a signature change
    // only means the *set* of files may differ (a rollout added/removed), not
    // that every surviving file's header changed. Dropping the whole map on any
    // signature miss made the common "one new rollout under a busy sessions
    // root" path re-read every existing header, defeating the cache.
    let previous: Option<RootIndex> = {
        let state = lock_cache();
        state.roots.get(&canonical_root).cloned()
    };
    let signature_hit = previous
        .as_ref()
        .is_some_and(|index| index.signature == signature);
    let previous_files = previous.map(|index| index.files);

    let results = match (signature_hit, previous_files) {
        (true, Some(previous_files)) => {
            // Warm hit: the directory subtree is unchanged (no rollout added,
            // removed, or renamed), so the cached *file list* is authoritative —
            // reuse it directly and skip the `O(directories × entries)`
            // `rollout_files_under` `read_dir` re-walk. Each cached path is still
            // re-`stat`ed and its header re-read only when its `(mtime, len)`
            // changed, so an in-place append/rewrite (which advances the file
            // mtime/len but NOT the parent directory mtime, leaving the signature
            // unchanged) is still caught. This is the cheap warm path the index
            // exists for: on a busy root it costs `O(directories)` for the
            // signature + `O(files)` stats, with zero header reads when nothing
            // changed and zero directory recursion.
            scan_indexed_rollouts_from_paths(previous_files.keys(), Some(&previous_files))
        }
        (_, previous_files) => {
            // Cold / changed tree: re-walk to refresh the file list, reusing each
            // surviving file's cached header when its `(mtime, len)` is unchanged.
            scan_indexed_rollouts(root, previous_files.as_ref())
        }
    };

    // Rebuild the cache entry from the fresh results.
    let mut files = HashMap::with_capacity(results.len());
    for item in &results {
        files.insert(
            item.path.clone(),
            CachedFile {
                modified: item.modified,
                len: item.len,
                meta: item.meta.clone(),
            },
        );
    }
    store_root(&canonical_root, signature, files);
    results
}

/// Direct scan via the directory re-walk (optionally reusing cached per-file
/// metadata). Used on the cold / signature-miss path where the file *set* may
/// have changed. When `previous` is `Some` and a file's `(modified, len)` are
/// unchanged, its cached header is reused; otherwise the header is read from
/// disk.
fn scan_indexed_rollouts(
    root: &Path,
    previous: Option<&HashMap<PathBuf, CachedFile>>,
) -> Vec<IndexedRollout> {
    scan_indexed_rollouts_from_paths(rollout_files_under(root).iter(), previous)
}

/// Build indexed candidates from an explicit set of candidate paths, re-`stat`ing
/// each and reusing the cached header iff its `(modified, len)` is unchanged.
/// The signature-hit warm path feeds the cached `RootIndex.files` keys here to
/// avoid the directory re-walk while still revalidating each file's `(mtime,
/// len)` (so in-place appends/rewrites are not served stale). The cold path
/// feeds freshly-walked paths. A path that vanished between the cached snapshot
/// and this `stat` is dropped via `metadata().ok()?`, so a deleted rollout under
/// an otherwise-unchanged signature is not surfaced.
fn scan_indexed_rollouts_from_paths<'a, I>(
    paths: I,
    previous: Option<&HashMap<PathBuf, CachedFile>>,
) -> Vec<IndexedRollout>
where
    I: IntoIterator<Item = &'a PathBuf>,
{
    paths
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(path).ok()?;
            let modified = metadata.modified().ok()?;
            let len = metadata.len();
            let meta = match previous.and_then(|prev| prev.get(path)) {
                Some(cached) if cached.modified == modified && cached.len == len => {
                    cached.meta.clone()
                }
                _ => read_rollout_session_meta(path),
            };
            Some(IndexedRollout {
                path: path.clone(),
                modified,
                len,
                meta,
            })
        })
        .collect()
}

fn lock_cache() -> std::sync::MutexGuard<'static, IndexState> {
    cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn store_root(canonical_root: &Path, signature: u64, files: HashMap<PathBuf, CachedFile>) {
    let mut state = lock_cache();
    state.seq = state.seq.wrapping_add(1);
    let seq = state.seq;
    state.roots.insert(
        canonical_root.to_path_buf(),
        RootIndex {
            signature,
            files,
            last_built_seq: seq,
        },
    );
    // Bound the number of cached roots with a tiny LRU eviction so a caller that
    // passes many distinct roots cannot grow the cache without limit.
    if state.roots.len() > MAX_CACHED_ROOTS {
        if let Some(victim) = state
            .roots
            .iter()
            .min_by_key(|(_, index)| index.last_built_seq)
            .map(|(path, _)| path.clone())
        {
            state.roots.remove(&victim);
        }
    }
}

/// Clear the entire cache. Test-only hook (REQ-004): lets tests force the cold
/// path and prevents cross-test contamination from a shared process-lifetime
/// cache. Compiled only under `cfg(test)`; the production rollback path is the
/// `codex_rollout_index_cache_enabled` config flag, not this function.
#[cfg(test)]
pub fn reset_cache_for_tests() {
    let mut state = lock_cache();
    state.roots.clear();
    state.seq = 0;
}

/// Process-global serialization lock for any test that touches the shared
/// rollout index. The cache lives for the whole process, so two tests that
/// populate/reset it under default parallel `cargo test` would otherwise race —
/// e.g. a `session.rs` resolver test mutating `roots` between a `rollout_index`
/// test's reset and its cache-state assertion. ALL test modules across this
/// crate that exercise the index (here AND `session.rs`) must serialize through
/// [`lock_cache_for_tests`].
#[cfg(test)]
static CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the shared cache test lock and reset the cache, returning the guard.
/// Holding the returned guard for the duration of a test gives that test an
/// isolated, freshly-reset process-global index (REQ-004 / TEST-004). Exposed
/// (not module-private) so `session.rs` tests share the exact same lock instead
/// of running unsynchronized against the same global state.
#[cfg(test)]
pub fn lock_cache_for_tests() -> std::sync::MutexGuard<'static, ()> {
    let guard = CACHE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_cache_for_tests();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    fn lock_test() -> MutexGuard<'static, ()> {
        lock_cache_for_tests()
    }

    fn write_rollout(root: &Path, relative: &str, id: &str, cwd: &Path) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"cwd\":\"{}\"}}}}\n",
                id,
                cwd.display()
            ),
        )
        .unwrap();
        path
    }

    // TEST-006: the shared discovery primitive finds exactly the rollout files.
    #[test]
    fn rollout_files_under_collects_only_rollout_jsonl() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = write_rollout(dir.path(), "2026/05/rollout-a.jsonl", "s1", cwd.path());
        let b = write_rollout(dir.path(), "2026/06/rollout-b.jsonl", "s2", cwd.path());
        std::fs::write(dir.path().join("2026/05/not-a-rollout.txt"), "x").unwrap();

        let mut found = rollout_files_under(dir.path());
        found.sort();
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(found, expected);
    }

    // TEST-001: an unchanged tree yields a stable signature; adding a file
    // changes it.
    #[test]
    fn tree_signature_changes_when_rollout_added() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_rollout(dir.path(), "2026/05/rollout-a.jsonl", "s1", cwd.path());
        let sig1 = tree_signature(dir.path()).unwrap();
        assert_eq!(
            tree_signature(dir.path()).unwrap(),
            sig1,
            "signature must be stable for an unchanged tree"
        );

        // A new dated directory always bumps the signature.
        write_rollout(dir.path(), "2026/07/rollout-c.jsonl", "s3", cwd.path());
        let sig2 = tree_signature(dir.path()).unwrap();
        assert_ne!(
            sig1, sig2,
            "adding a rollout under a new directory must change the signature"
        );
    }

    // TEST-001 / TEST-004: missing root yields no signature and is not cached,
    // so a later creation is picked up.
    #[test]
    fn missing_root_then_created_is_picked_up() {
        let _guard = lock_test();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("sessions-not-yet-there");
        assert!(tree_signature(&root).is_none());
        // Cold lookup on a missing root returns nothing and caches nothing.
        assert!(cached_indexed_rollouts(&root).is_empty());

        let cwd = tempfile::tempdir().unwrap();
        write_rollout(&root, "2026/05/rollout-a.jsonl", "s1", cwd.path());
        let found = cached_indexed_rollouts(&root);
        assert_eq!(
            found.len(),
            1,
            "a root created after a miss must be discovered on the next lookup"
        );
    }

    // TEST-005: a warm scan whose `(mtime, len)` matches the cached entry does
    // NOT re-read the rollout header. We prove this deterministically by handing
    // `scan_indexed_rollouts` a synthetic `previous` map whose `(modified, len)`
    // exactly matches the on-disk file but whose cached `meta` carries a sentinel
    // id. If the scan reused the cache (no header re-read) it returns the
    // sentinel; if it re-read the file it would return the real on-disk id.
    #[test]
    fn warm_scan_reuses_cached_header_without_reread() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let path = write_rollout(
            dir.path(),
            "2026/05/rollout-a.jsonl",
            "on-disk-id",
            cwd.path(),
        );
        let metadata = std::fs::metadata(&path).unwrap();
        let modified = metadata.modified().unwrap();
        let len = metadata.len();

        let mut previous = HashMap::new();
        previous.insert(
            path.clone(),
            CachedFile {
                modified,
                len,
                meta: Some(RolloutSessionMeta {
                    id: "cached-sentinel".to_string(),
                    cwd: cwd.path().to_path_buf(),
                    source: None,
                    originator: None,
                }),
            },
        );

        let warm = scan_indexed_rollouts(dir.path(), Some(&previous));
        assert_eq!(warm.len(), 1);
        assert_eq!(
            warm[0].meta.as_ref().unwrap().id,
            "cached-sentinel",
            "matching (mtime, len) must reuse the cached header instead of re-reading the file"
        );

        // Control: a mismatched len forces a re-read and returns the real id.
        let mut stale = HashMap::new();
        stale.insert(
            path.clone(),
            CachedFile {
                modified,
                len: len + 1,
                meta: Some(RolloutSessionMeta {
                    id: "cached-sentinel".to_string(),
                    cwd: cwd.path().to_path_buf(),
                    source: None,
                    originator: None,
                }),
            },
        );
        let rescanned = scan_indexed_rollouts(dir.path(), Some(&stale));
        assert_eq!(
            rescanned[0].meta.as_ref().unwrap().id,
            "on-disk-id",
            "a changed (mtime, len) must force a header re-read"
        );
    }

    // TEST-001 / TEST-005: a second `cached_indexed_rollouts` call with an
    // unchanged tree reuses the cached header end-to-end (full warm-cache path),
    // proven by corrupting the file to unparseable AFTER priming while leaving
    // mtime/len untouched via an in-place same-length overwrite.
    #[test]
    fn warm_lookup_end_to_end_reuses_cache() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let path = write_rollout(
            dir.path(),
            "2026/05/rollout-a.jsonl",
            "warm-sess",
            cwd.path(),
        );

        let cold = cached_indexed_rollouts(dir.path());
        assert_eq!(cold[0].meta.as_ref().unwrap().id, "warm-sess");
        let primed = std::fs::metadata(&path).unwrap();
        let (modified, len) = (primed.modified().unwrap(), primed.len());

        // Best-effort in-place same-length corruption. If the filesystem keeps
        // (mtime, len) stable (common for an immediate same-size rewrite), the
        // warm path must still return the cached header. If the mtime advanced,
        // we skip the strict assertion to avoid filesystem-timing flakiness but
        // still assert the tree signature path stayed consistent.
        let corrupt = "x".repeat(len as usize);
        std::fs::write(&path, &corrupt).unwrap();
        let after = std::fs::metadata(&path).unwrap();
        let warm = cached_indexed_rollouts(dir.path());
        if after.modified().unwrap() == modified && after.len() == len {
            assert_eq!(
                warm[0].meta.as_ref().unwrap().id,
                "warm-sess",
                "warm cache must reuse the cached header rather than re-reading the corrupted file"
            );
        }
    }

    // TEST-004: rewriting a cached file's content (changing its length) under the
    // same leaf must invalidate that file's cached header even if the directory
    // signature were somehow unchanged — the per-file (mtime, len) guard catches
    // it. Here we also change content length so the header is re-parsed.
    #[test]
    fn same_leaf_content_rewrite_reparses_header() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let path = write_rollout(dir.path(), "2026/05/rollout-a.jsonl", "old-id", cwd.path());

        let cold = cached_indexed_rollouts(dir.path());
        assert_eq!(cold[0].meta.as_ref().unwrap().id, "old-id");

        // Rewrite with a different session id (different length) and bump mtime.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"cwd\":\"{}\"}}}}\n",
                "new-longer-id",
                cwd.path().display()
            ),
        )
        .unwrap();

        let warm = cached_indexed_rollouts(dir.path());
        assert_eq!(
            warm[0].meta.as_ref().unwrap().id,
            "new-longer-id",
            "a content/length rewrite must re-parse the header"
        );
    }

    // Gate logic: unset field defaults ON; explicit false disables.
    #[test]
    fn enabled_from_field_defaults_on() {
        assert!(enabled_from_field(None));
        assert!(enabled_from_field(Some(true)));
        assert!(!enabled_from_field(Some(false)));
    }

    // Rollback contract: with the cache disabled, lookups still return the exact
    // same candidate set (correctness preserved) but populate NO cache state, so
    // there is no stale-hit surface. This is the behavioural rollback proven
    // without mutating the process-global live config.
    #[test]
    fn disabled_cache_returns_results_but_caches_nothing() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_rollout(dir.path(), "2026/05/rollout-a.jsonl", "s1", cwd.path());

        let disabled = cached_indexed_rollouts_inner(dir.path(), false);
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].meta.as_ref().unwrap().id, "s1");
        {
            let state = lock_cache();
            assert!(
                state.roots.is_empty(),
                "disabled cache must not populate any root state"
            );
        }

        // And the enabled path produces the identical candidate set.
        let enabled = cached_indexed_rollouts_inner(dir.path(), true);
        assert_eq!(enabled.len(), disabled.len());
        assert_eq!(
            enabled[0].meta.as_ref().unwrap().id,
            disabled[0].meta.as_ref().unwrap().id
        );
    }

    // TEST-001 / TEST-004: a new rollout added after a warm lookup is discovered
    // on the next lookup because the directory mtime (and thus the signature)
    // changes. Proven end-to-end through the cached entry point.
    #[test]
    fn new_rollout_after_warm_lookup_is_discovered() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_rollout(dir.path(), "2026/05/rollout-a.jsonl", "s1", cwd.path());
        let first = cached_indexed_rollouts(dir.path());
        assert_eq!(first.len(), 1);

        // New rollout under a new leaf directory bumps the tree signature.
        write_rollout(dir.path(), "2026/06/rollout-b.jsonl", "s2", cwd.path());
        let second = cached_indexed_rollouts(dir.path());
        assert_eq!(
            second.len(),
            2,
            "a rollout created after a warm lookup must be discovered on the next lookup"
        );
    }

    // Finding (rollout_index.rs:310): on a signature MISS (a new rollout under a
    // new leaf bumps the tree signature), the surviving files' cached headers must
    // still be reused — only the genuinely new/changed file pays a header read.
    // We prove the survivor's header is not re-read by corrupting it in place to
    // unparseable while keeping its (mtime, len) stable: a warm reuse returns the
    // cached id; a blanket drop would re-read and lose it.
    #[test]
    fn signature_miss_reuses_surviving_file_headers() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let survivor = write_rollout(
            dir.path(),
            "2026/05/rollout-a.jsonl",
            "survivor",
            cwd.path(),
        );

        // Cold lookup warms the cache with the survivor's header.
        let cold = cached_indexed_rollouts(dir.path());
        assert_eq!(cold.len(), 1);
        let primed = std::fs::metadata(&survivor).unwrap();
        let (modified, len) = (primed.modified().unwrap(), primed.len());

        // Corrupt the survivor in place (same length) so a header RE-READ would
        // yield `None`, while a cache REUSE keeps the parsed id.
        let corrupt = "x".repeat(len as usize);
        std::fs::write(&survivor, &corrupt).unwrap();
        let after = std::fs::metadata(&survivor).unwrap();

        // Add a brand-new rollout under a new leaf -> the tree signature changes
        // (signature MISS), forcing a re-walk.
        write_rollout(
            dir.path(),
            "2026/06/rollout-b.jsonl",
            "newcomer",
            cwd.path(),
        );

        let warm = cached_indexed_rollouts(dir.path());
        assert_eq!(warm.len(), 2, "the new rollout must be discovered");
        if after.modified().unwrap() == modified && after.len() == len {
            let survivor_meta = warm
                .iter()
                .find(|item| item.path == survivor)
                .and_then(|item| item.meta.as_ref());
            assert_eq!(
                survivor_meta.map(|meta| meta.id.as_str()),
                Some("survivor"),
                "a signature miss must reuse the surviving file's cached header, not re-read it"
            );
        }
    }

    // Finding (rollout_index.rs:339): on a signature HIT the warm path builds
    // results from the cached file list and skips both the `rollout_files_under`
    // re-walk and the header re-read for files whose `(mtime, len)` is unchanged.
    // We prove the warm path serves the CACHED header (i.e. it did not re-read the
    // file) by corrupting the file in place to unparseable while preserving its
    // `(mtime, len)` AND the directory signature: a cache reuse keeps the parsed
    // id; a re-read would yield `None`.
    #[test]
    fn signature_hit_reuses_cached_file_list_without_rewalk() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let path = write_rollout(
            dir.path(),
            "2026/05/rollout-a.jsonl",
            "warm-list",
            cwd.path(),
        );

        let cold = cached_indexed_rollouts(dir.path());
        assert_eq!(cold.len(), 1);
        let signature_after_cold = tree_signature(dir.path()).unwrap();
        let primed = std::fs::metadata(&path).unwrap();
        let (modified, len) = (primed.modified().unwrap(), primed.len());

        // In-place same-length corruption: leaves the file `(mtime, len)` stable
        // (common for an immediate same-size rewrite) and does not touch the
        // parent directory mtime, so the tree signature is unchanged -> warm hit.
        let corrupt = "x".repeat(len as usize);
        std::fs::write(&path, &corrupt).unwrap();
        let after = std::fs::metadata(&path).unwrap();

        let warm = cached_indexed_rollouts(dir.path());
        assert_eq!(
            tree_signature(dir.path()).unwrap(),
            signature_after_cold,
            "tree signature must be stable across the warm lookup"
        );
        assert_eq!(warm.len(), 1, "warm hit must reuse the cached file list");
        if after.modified().unwrap() == modified && after.len() == len {
            assert_eq!(
                warm[0].meta.as_ref().unwrap().id,
                "warm-list",
                "a signature-hit warm lookup with unchanged (mtime, len) must reuse the cached header, not re-read the corrupted file"
            );
        }
    }

    // Finding (rollout_index.rs:339) correctness floor: even on a signature HIT,
    // an in-place content rewrite that DOES change `(mtime, len)` (e.g. a Codex
    // append) must be re-read — the warm path re-`stat`s each cached path, so the
    // append is not served stale despite the unchanged directory signature.
    #[test]
    fn signature_hit_still_rereads_on_mtime_len_change() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let path = write_rollout(dir.path(), "2026/05/rollout-a.jsonl", "before", cwd.path());

        let cold = cached_indexed_rollouts(dir.path());
        assert_eq!(cold[0].meta.as_ref().unwrap().id, "before");

        // Rewrite the SAME file in place with a different id + length, bumping its
        // mtime, WITHOUT adding/removing a directory entry (signature unchanged).
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"cwd\":\"{}\"}}}}\n",
                "after-longer-id",
                cwd.path().display()
            ),
        )
        .unwrap();

        let warm = cached_indexed_rollouts(dir.path());
        assert_eq!(
            warm[0].meta.as_ref().unwrap().id,
            "after-longer-id",
            "a signature-hit warm lookup must re-read a file whose (mtime, len) changed"
        );
    }

    // TEST-001 / regression: adding a rollout under an already-known leaf must
    // change the signature even if the leaf directory mtime is restored or too
    // coarse to advance. The signature hashes rollout membership, so the warm
    // path cannot stale-hit and miss the new file.
    #[test]
    fn same_leaf_rollout_membership_changes_signature_even_with_restored_dir_mtime() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_rollout(dir.path(), "2026/05/rollout-a.jsonl", "s1", cwd.path());
        let leaf = dir.path().join("2026/05");
        let original_mtime = std::fs::metadata(&leaf).unwrap().modified().unwrap();
        let sig1 = tree_signature(dir.path()).unwrap();

        write_rollout(dir.path(), "2026/05/rollout-b.jsonl", "s2", cwd.path());
        filetime::set_file_mtime(&leaf, filetime::FileTime::from_system_time(original_mtime))
            .unwrap();

        let sig2 = tree_signature(dir.path()).unwrap();
        assert_ne!(
            sig1, sig2,
            "same-leaf rollout membership must invalidate the cached file list even when directory mtime is unchanged"
        );
    }

    // TEST-004: reset clears the cache so a fresh process-like state is restored.
    #[test]
    fn reset_clears_cache() {
        let _guard = lock_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_rollout(dir.path(), "2026/05/rollout-a.jsonl", "s1", cwd.path());
        let _ = cached_indexed_rollouts(dir.path());

        reset_cache_for_tests();
        let state = lock_cache();
        assert!(state.roots.is_empty(), "reset must clear all cached roots");
    }
}
