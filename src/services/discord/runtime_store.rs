use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(super) fn agentdesk_root() -> Option<PathBuf> {
    #[cfg(test)]
    {
        test_agentdesk_root()
    }
    #[cfg(not(test))]
    {
        crate::config::runtime_root()
    }
}

#[cfg(test)]
fn test_agentdesk_root() -> Option<PathBuf> {
    if let Ok(override_root) = std::env::var("AGENTDESK_ROOT_DIR") {
        let trimmed = override_root.trim();
        if !trimmed.is_empty() {
            let root = PathBuf::from(trimmed);
            if !is_live_release_runtime_root(&root) {
                return Some(root);
            }
        }
    }
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    Some(
        ROOT.get_or_init(|| tempfile::tempdir().expect("create isolated test runtime root"))
            .path()
            .to_path_buf(),
    )
}

#[cfg(test)]
fn is_live_release_runtime_root(root: &Path) -> bool {
    dirs::home_dir().is_some_and(|home| root == home.join(".adk").join("release"))
}

pub(super) fn runtime_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("runtime"))
}

pub(super) fn workspace_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("workspaces"))
}

pub(super) fn worktrees_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("worktrees"))
}

pub(super) fn bot_settings_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::config_dir(&root).join("bot_settings.json"))
}

pub(super) fn role_map_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::role_map_path(&root))
}

pub(super) fn org_schema_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| org_schema_path_for_root(&root))
}

pub(crate) fn org_schema_path_for_root(root: &Path) -> PathBuf {
    crate::runtime_layout::org_schema_path(root)
}

pub(super) fn discord_uploads_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_uploads"))
}

pub(super) fn discord_inflight_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_inflight"))
}

pub(super) fn discord_restart_reports_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_restart_reports"))
}

/// #4049 S4-a1: durable turn-view reaction state. The reconciler stores the
/// bot token hash that added a lifecycle reaction so cold terminal/clear
/// notifications after restart remove with the same Discord @me identity.
pub(super) fn discord_turn_view_reconciler_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_turn_view_reconciler"))
}

/// #3293 verify r1 (finding 3): durable preservation of the full assistant
/// response + row metadata for every recovery force-clear. Kept OUT of
/// `discord_restart_reports/` because that store is flushed-and-deleted on
/// boot; these files are operator-recovery artifacts and are never GC'd.
pub(super) fn discord_recovery_force_clear_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_recovery_force_clear"))
}

pub(crate) fn discord_pending_queue_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_pending_queue"))
}

/// #3154: durable store for TUI-direct synthetic turn-starts that must be
/// claimed only AFTER the prior turn on the same channel finalizes. A wakeup/
/// loop turn writes one record here (before any wait); a detached per-channel
/// worker claims it post-drain and deletes the record. Restored on startup so a
/// dcserver restart mid-wait neither loses the turn nor resubmits the prompt.
pub(crate) fn tui_direct_pending_start_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_tui_direct_pending_start"))
}

/// #3296: durable aborted-anchor markers for TUI-direct synthetic turn-starts
/// that ABORTed after the input was already provider-submitted. The anchor
/// keeps its `⏳`; the watcher terminal-commit drain flips it to `✅` when the
/// prior owner covers it, and the placeholder sweeper flips it to `⚠` after
/// the TTL when nothing did. See `tui_direct_abort_marker`.
pub(crate) fn tui_direct_abort_marker_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_tui_direct_abort_marker"))
}

/// #3296 codex r2: durable terminal-commit tombstones for `(provider, tmux,
/// channel)`. The tmux watcher's terminal-commit chokepoint records one BEFORE
/// it clears the inflight row, so the aborted-anchor reconcilers can
/// distinguish "the foreign row vanished because its owner committed" (`✅`)
/// from a non-commit deletion (force-clear/stop/recovery → bounded `⚠`).
/// Short-lived: the marker sweep GC's tombstones past the marker hard cap.
pub(crate) fn tui_direct_commit_tombstone_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_tui_direct_commit_tombstone"))
}

/// #3003: durable retry store for orphaned status-panel-v2 message deletes that
/// failed transiently when no per-turn inflight handle survived (e.g. a
/// stopped/cancelled TUI-direct turn). Drained by the placeholder sweeper.
pub(super) fn discord_status_panel_orphans_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_status_panel_orphans"))
}

/// #4860: durable current-panel binding for the two-message singleton. Unlike
/// the orphan store, this record intentionally survives normal turn completion.
pub(super) fn discord_status_panel_singletons_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_status_panel_singletons"))
}

/// #4888: durable per-input busy-notice binding and aggregate retry budget.
pub(super) fn discord_busy_followup_retries_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_busy_followup_retries"))
}

/// #3859: durable abandon-request store. A SYNC failure-path site (turn-task
/// `InflightCleanupGuard` Drop, heartbeat-gap sweeper) that evicts an inflight
/// row with a live "🔄 처리 중" placeholder cannot drive the async Discord edit
/// itself, and deleting the row strands the placeholder forever. Instead it
/// records `(channel_id, placeholder_msg_id, started_at, current_tool_line)`
/// here — independent of the inflight lifecycle — and deletes the row
/// immediately (freeing the channel, like the pre-#3859 path). The placeholder
/// sweeper drains this store and finalizes each placeholder to its terminal
/// "중단됨" card BY MESSAGE ID. Mirrors `discord_status_panel_orphans`.
pub(super) fn discord_abandon_requests_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_abandon_requests"))
}

/// #3607: durable UI-only obligations for terminal-delivered turns whose TUI
/// quiescence gate timed out after the answer was already committed. This store
/// owns only status-card edits; it is intentionally separate from inflight and
/// delivery-record relay frontiers.
pub(crate) fn discord_terminal_ui_obligations_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_terminal_ui_obligations"))
}

/// #1332 round-3 codex review P2: per-channel sidecar root for the
/// `queued_placeholders` mapping. Persisted next to `discord_pending_queue/`
/// so a dcserver restart can re-attach restored mailbox queue entries to the
/// existing `📬 메시지 대기 중` Discord card instead of leaking a stale card
/// and posting a fresh placeholder.
pub(crate) fn discord_queued_placeholders_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_queued_placeholders"))
}

/// #1362: sidecar for queued placeholder cards that exited the queue before
/// the Serenity context was available. The regular queued-placeholder mapping
/// is already drained at queue-exit time; this store preserves the visible card
/// ids until the cached Discord HTTP client can delete them.
pub(crate) fn discord_queue_exit_placeholder_clears_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_queue_exit_placeholder_clears"))
}

/// Retired durable handoff directory. Kept only so startup can remove
/// legacy JSON records from builds that had a reader but no live writer.
pub(super) fn legacy_discord_handoff_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_handoff"))
}

pub(super) fn shared_agent_knowledge_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::shared_agent_knowledge_path(&root))
}

pub(super) fn long_term_memory_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::long_term_memory_root(&root))
}

/// Path to the generation counter file.
pub fn generation_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("runtime").join("generation"))
}

// <epoch-provenance-surface>

/// How this process's generation was selected; variants describe observed calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum GenerationAllocationRoute {
    /// `read_generation_counter` returned `Parsed` or `Absent`, `atomic_write`
    /// returned `Ok`, `next != current`, and `fsync_parent_dir` returned `Ok`.
    AdvancedWithSyncedRename,
    /// `atomic_write` returned `Ok`, but `fsync_parent_dir` returned `Err`.
    ParentSyncFailed,
    /// `atomic_write` and `fsync_parent_dir` returned `Ok`, but
    /// `read_generation_counter` returned `Failed`.
    CounterReadFailed,
    /// `atomic_write` returned `Ok`, but `next == current`; parent sync skipped.
    Saturated,
    /// `atomic_write` returned `Err`.
    WriteFailed,
    /// `lock_generation_path` returned `Err`; an unsynchronized
    /// `read_generation_counter` followed and no write was attempted.
    LockFailed,
    /// No generation path was available; no filesystem call was attempted.
    PathUnavailable,
    /// No transaction is bound, or a test pin supplied no provenance.
    Unwitnessed,
}

/// Generation and allocation route are published as one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct ProcessGenerationAllocation {
    pub(in crate::services::discord) generation: u64,
    route: GenerationAllocationRoute,
}

/// Counter reads retain establishment and bounded failure detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CounterReadFailure {
    Io(std::io::ErrorKind),
    Parse,
}

impl CounterReadFailure {
    fn as_str(self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Parse => "parse",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CounterRead {
    Parsed(u64),
    Absent,
    Failed(CounterReadFailure),
}

impl CounterRead {
    fn value(self) -> u64 {
        match self {
            Self::Parsed(value) => value,
            Self::Absent | Self::Failed(_) => 0,
        }
    }

    fn established(self) -> bool {
        !matches!(self, Self::Failed(_))
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Parsed(_) => "parsed",
            Self::Absent => "absent",
            Self::Failed(CounterReadFailure::Io(_)) => "io_error",
            Self::Failed(CounterReadFailure::Parse) => "parse_error",
        }
    }

    fn failure(self) -> Option<CounterReadFailure> {
        match self {
            Self::Failed(failure) => Some(failure),
            Self::Parsed(_) | Self::Absent => None,
        }
    }
}

impl GenerationAllocationRoute {
    fn advanced(self) -> bool {
        matches!(self, Self::AdvancedWithSyncedRename)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AdvancedWithSyncedRename => "advanced_with_synced_rename",
            Self::ParentSyncFailed => "parent_sync_failed",
            Self::CounterReadFailed => "counter_read_failed",
            Self::Saturated => "saturated",
            Self::WriteFailed => "write_failed",
            Self::LockFailed => "lock_failed",
            Self::PathUnavailable => "path_unavailable",
            Self::Unwitnessed => "unwitnessed",
        }
    }
}

impl ProcessGenerationAllocation {
    pub(in crate::services::discord) fn epoch_advanced(self) -> bool {
        self.route.advanced()
    }

    pub(in crate::services::discord) fn epoch_route(self) -> &'static str {
        self.route.as_str()
    }
}

#[cfg(test)]
mod test_generation_publication {
    use super::*;

    std::thread_local! {
        static BINDING: std::cell::Cell<Option<ProcessGenerationAllocation>> =
            const { std::cell::Cell::new(None) };
    }

    pub(in crate::services::discord) struct Publication {
        previous_binding: Option<ProcessGenerationAllocation>,
    }

    impl Drop for Publication {
        fn drop(&mut self) {
            BINDING.set(self.previous_binding);
        }
    }

    pub(super) fn published_binding() -> Option<ProcessGenerationAllocation> {
        BINDING.get()
    }

    pub(in crate::services::discord) fn allocation(
        generation: u64,
        route: GenerationAllocationRoute,
    ) -> ProcessGenerationAllocation {
        ProcessGenerationAllocation { generation, route }
    }

    fn parent_sync_failure(_: &Path) -> std::io::Result<()> {
        Err(std::io::Error::other("injected parent sync failure"))
    }

    pub(in crate::services::discord) fn allocate_and_publish(
        parent_sync_succeeds: bool,
    ) -> Publication {
        let _allocation = lock_unpoisoned(&PROCESS_GENERATION_ALLOCATION_TRANSACTION);
        let allocated = allocate_generation_epoch_with_io(GenerationIo {
            path: generation_path(),
            lock: lock_generation_path,
            read: read_generation_counter,
            write: atomic_write,
            fsync: if parent_sync_succeeds {
                fsync_parent_dir
            } else {
                parent_sync_failure
            },
        });
        let previous_binding = BINDING.replace(Some(allocated));
        Publication { previous_binding }
    }
}

#[cfg(test)]
pub(in crate::services::discord) use test_generation_publication::{
    allocate_and_publish as allocate_and_publish_process_generation_for_tests,
    allocation as process_generation_allocation_for_tests,
};

// </epoch-provenance-surface>

/// Load the current generation counter (returns 0 if missing or unreadable).
pub fn load_generation() -> u64 {
    generation_path().map_or(0, |path| read_generation_counter(&path).value())
}

/// Production publishes generation and route in one lock-free read cell.
#[cfg(not(test))]
static PROCESS_GENERATION: std::sync::OnceLock<ProcessGenerationAllocation> =
    std::sync::OnceLock::new();
#[cfg(test)]
static TEST_PROCESS_GENERATION_OVERRIDE: std::sync::Mutex<Option<u64>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_PROCESS_GENERATION_ALLOCATION: std::sync::Mutex<Option<ProcessGenerationAllocation>> =
    std::sync::Mutex::new(None);
static PROCESS_GENERATION_ALLOCATION_TRANSACTION: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn lock_unpoisoned<T>(cell: &'static std::sync::Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    cell.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
fn test_generation_override() -> std::sync::MutexGuard<'static, Option<u64>> {
    lock_unpoisoned(&TEST_PROCESS_GENERATION_OVERRIDE)
}

#[cfg(test)]
fn test_allocation_memo() -> std::sync::MutexGuard<'static, Option<ProcessGenerationAllocation>> {
    lock_unpoisoned(&TEST_PROCESS_GENERATION_ALLOCATION)
}

fn allocate_once_with(
    cell: &std::sync::OnceLock<ProcessGenerationAllocation>,
    transaction: &std::sync::Mutex<()>,
    allocate: impl FnOnce() -> ProcessGenerationAllocation,
) -> ProcessGenerationAllocation {
    if let Some(bound) = cell.get() {
        return *bound;
    }
    let _transaction = transaction
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *cell.get_or_init(allocate)
}

struct GenerationFileLock {
    _file: fs::File,
}

impl Drop for GenerationFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn lock_generation_path(path: &Path) -> Result<GenerationFileLock, String> {
    let lock_path = path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(GenerationFileLock { _file: file })
}

struct GenerationIo<L, R, W, F> {
    path: Option<PathBuf>,
    lock: L,
    read: R,
    write: W,
    fsync: F,
}

/// Allocate once, publishing generation and route together.
pub fn allocate_process_generation() -> u64 {
    allocate_process_generation_binding().generation
}

pub(in crate::services::discord) fn allocate_process_generation_binding()
-> ProcessGenerationAllocation {
    #[cfg(not(test))]
    {
        allocate_once_with(
            &PROCESS_GENERATION,
            &PROCESS_GENERATION_ALLOCATION_TRANSACTION,
            allocate_generation_epoch,
        )
    }
    #[cfg(test)]
    {
        if let Some(generation) = *test_generation_override() {
            return ProcessGenerationAllocation {
                generation,
                route: GenerationAllocationRoute::Unwitnessed,
            };
        }
        if let Some(bound) = *test_allocation_memo() {
            return bound;
        }
        let _allocation = lock_unpoisoned(&PROCESS_GENERATION_ALLOCATION_TRANSACTION);
        if let Some(generation) = *test_generation_override() {
            return ProcessGenerationAllocation {
                generation,
                route: GenerationAllocationRoute::Unwitnessed,
            };
        }
        if let Some(bound) = *test_allocation_memo() {
            return bound;
        }
        let allocated = allocate_generation_epoch();
        *test_allocation_memo() = Some(allocated);
        allocated
    }
}

fn allocate_generation_epoch() -> ProcessGenerationAllocation {
    allocate_generation_epoch_with_io(GenerationIo {
        path: generation_path(),
        lock: lock_generation_path,
        read: read_generation_counter,
        write: atomic_write,
        fsync: fsync_parent_dir,
    })
}

fn read_generation_counter(path: &Path) -> CounterRead {
    match fs::read_to_string(path) {
        Ok(body) => match body.trim().parse::<u64>() {
            Ok(value) => CounterRead::Parsed(value),
            Err(_) => CounterRead::Failed(CounterReadFailure::Parse),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CounterRead::Absent,
        Err(error) => CounterRead::Failed(CounterReadFailure::Io(error.kind())),
    }
}

fn allocate_generation_epoch_with_io<L, R, W, F>(
    io: GenerationIo<L, R, W, F>,
) -> ProcessGenerationAllocation
where
    L: FnOnce(&Path) -> Result<GenerationFileLock, String>,
    R: FnOnce(&Path) -> CounterRead,
    W: FnOnce(&Path, &str) -> Result<(), String>,
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    use GenerationAllocationRoute as Route;

    let Some(path) = io.path else {
        tracing::warn!(
            path = "<unavailable>",
            current = 0u64,
            next = 0u64,
            counter_read = "not_attempted",
            counter_detail = "not_attempted",
            route = Route::PathUnavailable.as_str(),
            epoch_advanced = false,
            "runtime generation counter path is unavailable; allocation was not attempted"
        );
        return ProcessGenerationAllocation {
            generation: 0,
            route: Route::PathUnavailable,
        };
    };

    let _lock = match (io.lock)(&path) {
        Ok(lock) => lock,
        Err(error) => {
            let read = (io.read)(&path);
            let generation = read.value();
            tracing::error!(
                path = %path.display(),
                current = generation,
                next = generation,
                counter_read = read.as_str(),
                counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                error = %error,
                route = Route::LockFailed.as_str(),
                epoch_advanced = false,
                "failed to lock runtime generation counter"
            );
            return ProcessGenerationAllocation {
                generation,
                route: Route::LockFailed,
            };
        }
    };

    let read = (io.read)(&path);
    let current = read.value();
    let next = current.saturating_add(1);
    match (io.write)(&path, &next.to_string()) {
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                current,
                next,
                counter_read = read.as_str(),
                counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                error = %error,
                route = Route::WriteFailed.as_str(),
                epoch_advanced = false,
                "failed to allocate runtime process generation"
            );
            ProcessGenerationAllocation {
                generation: current,
                route: Route::WriteFailed,
            }
        }
        Ok(()) if next == current => {
            tracing::error!(
                path = %path.display(),
                current,
                next,
                counter_read = read.as_str(),
                counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                route = Route::Saturated.as_str(),
                epoch_advanced = false,
                "runtime process generation counter is saturated"
            );
            ProcessGenerationAllocation {
                generation: next,
                route: Route::Saturated,
            }
        }
        Ok(()) => match (io.fsync)(&path) {
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    current,
                    next,
                    counter_read = read.as_str(),
                    counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                    error = %error,
                    route = Route::ParentSyncFailed.as_str(),
                    epoch_advanced = false,
                    "runtime generation counter was renamed but parent-directory sync returned an error"
                );
                ProcessGenerationAllocation {
                    generation: next,
                    route: Route::ParentSyncFailed,
                }
            }
            Ok(()) if !read.established() => {
                tracing::warn!(
                    path = %path.display(),
                    current,
                    next,
                    counter_read = read.as_str(),
                    counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                    route = Route::CounterReadFailed.as_str(),
                    epoch_advanced = false,
                    "runtime generation counter was renamed, but the prior value was not established"
                );
                ProcessGenerationAllocation {
                    generation: next,
                    route: Route::CounterReadFailed,
                }
            }
            Ok(()) => {
                tracing::info!(
                    path = %path.display(),
                    current,
                    next,
                    counter_read = read.as_str(),
                    counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                    route = Route::AdvancedWithSyncedRename.as_str(),
                    epoch_advanced = true,
                    "allocated runtime process generation"
                );
                ProcessGenerationAllocation {
                    generation: next,
                    route: Route::AdvancedWithSyncedRename,
                }
            }
        },
    }
}

/// Return the process generation; before boot allocation, use the on-disk
/// counter for constructor-only compatibility.
pub fn process_generation() -> u64 {
    #[cfg(not(test))]
    {
        process_generation_binding().generation
    }
    #[cfg(test)]
    {
        if let Some(generation) = *test_generation_override() {
            return generation;
        }
        load_generation()
    }
}

pub(in crate::services::discord) fn process_generation_binding() -> ProcessGenerationAllocation {
    #[cfg(not(test))]
    if let Some(bound) = PROCESS_GENERATION.get() {
        return *bound;
    }
    #[cfg(test)]
    {
        if let Some(generation) = *test_generation_override() {
            return ProcessGenerationAllocation {
                generation,
                route: GenerationAllocationRoute::Unwitnessed,
            };
        }
        if let Some(bound) = test_generation_publication::published_binding() {
            return bound;
        }
    }
    ProcessGenerationAllocation {
        generation: load_generation(),
        route: GenerationAllocationRoute::Unwitnessed,
    }
}

/// Preview the epoch that the replacement process will allocate without
/// mutating the on-disk counter while the old process is still quiescing.
pub fn next_process_generation() -> u64 {
    load_generation().saturating_add(1)
}

#[cfg(test)]
pub fn set_process_generation_for_tests(generation: Option<u64>) {
    let _allocation = lock_unpoisoned(&PROCESS_GENERATION_ALLOCATION_TRANSACTION);
    *test_generation_override() = generation;
    if generation.is_none() {
        *test_allocation_memo() = None;
    }
}

pub(super) fn last_message_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("last_message"))
}

struct LastMessageIdFileLock {
    _file: fs::File,
}

impl Drop for LastMessageIdFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn last_message_id_lock_path(path: &Path) -> PathBuf {
    path.with_extension("txt.lock")
}

fn lock_last_message_id_path(path: &Path) -> Result<LastMessageIdFileLock, String> {
    let lock_path = last_message_id_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(LastMessageIdFileLock { _file: file })
}

fn read_last_message_id(path: &Path) -> Option<u64> {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u64>().ok())
}

/// Save the last processed message ID for a channel.
pub(super) fn save_last_message_id(provider: &str, channel_id: u64, message_id: u64) {
    let Some(root) = last_message_root() else {
        return;
    };
    let dir = root.join(provider);
    let path = dir.join(format!("{}.txt", channel_id));
    let Ok(_lock) = lock_last_message_id_path(&path) else {
        tracing::warn!(
            provider = provider,
            channel_id = channel_id,
            "last-message checkpoint save skipped because the file lock could not be acquired"
        );
        return;
    };
    let checkpoint = read_last_message_id(&path)
        .map(|existing| existing.max(message_id))
        .unwrap_or(message_id);
    best_effort_atomic_write_logged(
        &path,
        &checkpoint.to_string(),
        AtomicWriteContext::new("last_message")
            .provider(provider)
            .channel_id(channel_id),
    );
}

/// Save all last_message_ids from a map (used during SIGTERM).
pub(super) fn save_all_last_message_ids(provider: &str, ids: &std::collections::HashMap<u64, u64>) {
    for (channel_id, message_id) in ids {
        save_last_message_id(provider, *channel_id, *message_id);
    }
}

/// `errno` value for ENOSPC on both Linux and macOS.
const ENOSPC: i32 = 28;

/// Wrap an `io::Error` into a `String` while flagging ENOSPC out-of-band.
///
/// `runtime_store::atomic_write` is called from many sites that just want a
/// `Result<(), String>` so we keep the existing error shape, but we also
/// stamp `disk_monitor::record_enospc_now` whenever the underlying error is
/// "no space left on device". The monitoring tick then shows a banner even
/// though the per-call site stays oblivious (#1203 follow-up).
fn classify_io_error(prefix: &str, error: std::io::Error) -> String {
    if error.raw_os_error() == Some(ENOSPC) {
        crate::services::disk_monitor::record_enospc_now();
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] 💾 ENOSPC at runtime_store::atomic_write ({prefix}): {error}");
        format!("ENOSPC: {prefix}: {error}")
    } else {
        format!("{prefix}: {error}")
    }
}

fn discord_inflight_atomic_replace_channel_id(path: &Path) -> u64 {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.parse::<u64>().ok())
        .unwrap_or(0)
}

fn discord_inflight_atomic_replace_user_msg_id(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .and_then(|value| value.get("user_msg_id").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

fn log_discord_inflight_atomic_replace(path: &Path) {
    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
        return;
    }
    let Some(provider_dir) = path.parent() else {
        return;
    };
    if provider_dir
        .parent()
        .and_then(|root| root.file_name())
        .and_then(|name| name.to_str())
        != Some("discord_inflight")
    {
        return;
    }
    if !path.exists() {
        return;
    }
    let provider = provider_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    // NOT a removal: `atomic_write` only ever replaces the row's CONTENT (it
    // takes a serialized body and renames a temp file over the path). Logging
    // this under `agentdesk::inflight_remove` / "row removal" made routine
    // per-tick saves read as row deletions and sent operators down a false
    // trail, so the target and the message name the real operation.
    tracing::info!(
        target: "agentdesk::inflight_write",
        provider = %provider,
        channel_id = discord_inflight_atomic_replace_channel_id(path),
        user_msg_id = discord_inflight_atomic_replace_user_msg_id(path),
        reason = "runtime_store_atomic_write_replace",
        path = %path.display(),
        "discord inflight state row content replaced in place (not removed)"
    );
}

pub(crate) fn atomic_write(path: &Path, data: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| classify_io_error("create_dir_all", e))?;
    }
    let unique = uuid::Uuid::new_v4().simple();
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{}.{}.tmp", file_name, unique));
    let mut file = fs::File::create(&tmp).map_err(|e| classify_io_error("create_tmp", e))?;
    file.write_all(data.as_bytes())
        .map_err(|e| classify_io_error("write_all", e))?;
    file.sync_all()
        .map_err(|e| classify_io_error("sync_all", e))?;
    log_discord_inflight_atomic_replace(path);
    fs::rename(&tmp, path).map_err(|e| classify_io_error("rename", e))
}

/// #5254 D10: fsync the directory entry `atomic_write` published, so the rename
/// itself — not just the bytes `sync_all` already flushed — is handed to the
/// filesystem. The contract stops at the calls: `sync_all` on the file, then an
/// fsync of the parent directory. What either one survives is not a claim this
/// helper makes.
///
/// Deliberately a second call rather than a flag on `atomic_write`: the inflight
/// hot path must not pay a directory fsync, and callers need rename and parent
/// sync failures separable. Gateway identity recovery skips its derived index
/// on failure; generation allocation records a non-advanced route after its
/// rename. The directory is opened read-only because opening one for writing
/// fails with `EISDIR`.
pub(crate) fn fsync_parent_dir(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    fs::File::open(parent.unwrap_or_else(|| Path::new(".")))?.sync_all()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AtomicWriteContext<'a> {
    pub(crate) store: &'a str,
    pub(crate) provider: Option<&'a str>,
    pub(crate) token_hash: Option<&'a str>,
    pub(crate) channel_id: Option<u64>,
    pub(crate) session_key: Option<&'a str>,
    pub(crate) turn_id: Option<&'a str>,
}

impl<'a> AtomicWriteContext<'a> {
    pub(crate) fn new(store: &'a str) -> Self {
        Self {
            store,
            provider: None,
            token_hash: None,
            channel_id: None,
            session_key: None,
            turn_id: None,
        }
    }

    pub(crate) fn provider(mut self, provider: &'a str) -> Self {
        self.provider = Some(provider);
        self
    }

    pub(crate) fn token_hash(mut self, token_hash: &'a str) -> Self {
        self.token_hash = Some(token_hash);
        self
    }

    pub(crate) fn channel_id(mut self, channel_id: u64) -> Self {
        self.channel_id = Some(channel_id);
        self
    }
}

/// Recovery-critical writes must be visible when they fail because startup
/// reconciliation depends on their last durable snapshot.
pub(crate) fn critical_atomic_write(
    path: &Path,
    data: &str,
    context: AtomicWriteContext<'_>,
) -> Result<(), String> {
    atomic_write(path, data).map_err(|error| {
        tracing::error!(
            store = context.store,
            path = %path.display(),
            provider = ?context.provider,
            token_hash = ?context.token_hash,
            channel_id = ?context.channel_id,
            session_key = ?context.session_key,
            turn_id = ?context.turn_id,
            error = %error,
            "recovery-critical atomic write failed"
        );
        error
    })
}

/// Best-effort snapshots may not abort their caller, but failures should still
/// be observable in structured logs.
pub(crate) fn best_effort_atomic_write_logged(
    path: &Path,
    data: &str,
    context: AtomicWriteContext<'_>,
) {
    if let Err(error) = atomic_write(path, data) {
        tracing::warn!(
            store = context.store,
            path = %path.display(),
            provider = ?context.provider,
            token_hash = ?context.token_hash,
            channel_id = ?context.channel_id,
            session_key = ?context.session_key,
            turn_id = ?context.turn_id,
            error = %error,
            "best-effort atomic write failed"
        );
    }
}

#[cfg(test)]
mod generation_allocation_tests {
    use super::*;
    use std::sync::Arc;
    use tracing_subscriber::fmt::writer::MakeWriter;

    #[derive(Clone)]
    struct CapturingWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for CapturingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn io<F>(
        path: PathBuf,
        fsync: F,
    ) -> GenerationIo<
        impl FnOnce(&Path) -> Result<GenerationFileLock, String>,
        impl FnOnce(&Path) -> CounterRead,
        impl FnOnce(&Path, &str) -> Result<(), String>,
        F,
    >
    where
        F: FnOnce(&Path) -> std::io::Result<()>,
    {
        GenerationIo {
            path: Some(path),
            lock: lock_generation_path,
            read: read_generation_counter,
            write: atomic_write,
            fsync,
        }
    }

    fn expect(
        binding: ProcessGenerationAllocation,
        generation: u64,
        route: GenerationAllocationRoute,
    ) {
        assert_eq!(binding, ProcessGenerationAllocation { generation, route });
        assert_eq!(binding.epoch_route(), route.as_str());
        assert_eq!(
            binding.epoch_advanced(),
            matches!(route, GenerationAllocationRoute::AdvancedWithSyncedRename)
        );
    }

    fn fail_lock(_: &Path) -> Result<GenerationFileLock, String> {
        Err("lock".into())
    }

    fn panic_lock(_: &Path) -> Result<GenerationFileLock, String> {
        panic!("lock")
    }

    fn panic_read(_: &Path) -> CounterRead {
        panic!("read")
    }

    fn fail_write(_: &Path, _: &str) -> Result<(), String> {
        Err("write".into())
    }

    fn panic_write(_: &Path, _: &str) -> Result<(), String> {
        panic!("write")
    }

    fn panic_fsync(_: &Path) -> std::io::Result<()> {
        panic!("fsync")
    }

    #[test]
    fn core_truth_table_routes_and_side_effects_are_exact() {
        let root = tempfile::tempdir().unwrap();
        let seeded = |name: &str, value: &str| {
            let path = root.path().join(name).join("generation");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, value).unwrap();
            path
        };
        let path = seeded("parent-failed", "7");
        expect(
            allocate_generation_epoch_with_io(io(path.clone(), |_| {
                Err(std::io::Error::from(std::io::ErrorKind::Other))
            })),
            8,
            GenerationAllocationRoute::ParentSyncFailed,
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "8");
        expect(
            allocate_generation_epoch_with_io(io(seeded("parsed", "7"), |_| Ok(()))),
            8,
            GenerationAllocationRoute::AdvancedWithSyncedRename,
        );
        expect(
            allocate_generation_epoch_with_io(io(
                root.path().join("absent").join("generation"),
                |_| Ok(()),
            )),
            1,
            GenerationAllocationRoute::AdvancedWithSyncedRename,
        );
        let path = seeded("unreadable", "nan");
        expect(
            allocate_generation_epoch_with_io(io(path.clone(), |_| Ok(()))),
            1,
            GenerationAllocationRoute::CounterReadFailed,
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "1");
        expect(
            allocate_generation_epoch_with_io(io(seeded("unreadable-parent", "nan"), |_| {
                Err(std::io::Error::from(std::io::ErrorKind::Other))
            })),
            1,
            GenerationAllocationRoute::ParentSyncFailed,
        );

        let path = seeded("lock-failed", "7");
        expect(
            allocate_generation_epoch_with_io(GenerationIo {
                path: Some(path.clone()),
                lock: fail_lock,
                read: read_generation_counter,
                write: panic_write,
                fsync: panic_fsync,
            }),
            7,
            GenerationAllocationRoute::LockFailed,
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "7");
        let path = seeded("write-failed", "7");
        expect(
            allocate_generation_epoch_with_io(GenerationIo {
                path: Some(path.clone()),
                lock: lock_generation_path,
                read: read_generation_counter,
                write: fail_write,
                fsync: panic_fsync,
            }),
            7,
            GenerationAllocationRoute::WriteFailed,
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "7");
        let path = seeded("saturated", &u64::MAX.to_string());
        expect(
            allocate_generation_epoch_with_io(io(path.clone(), |_| panic!("fsync"))),
            u64::MAX,
            GenerationAllocationRoute::Saturated,
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), u64::MAX.to_string());
        expect(
            allocate_generation_epoch_with_io(GenerationIo {
                path: None,
                lock: panic_lock,
                read: panic_read,
                write: panic_write,
                fsync: panic_fsync,
            }),
            0,
            GenerationAllocationRoute::PathUnavailable,
        );
    }

    #[test]
    fn production_shaped_once_cell_publishes_generation_and_route_together() {
        let cell = std::sync::OnceLock::new();
        let transaction = std::sync::Mutex::new(());
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let allocate = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ProcessGenerationAllocation {
                generation: 42,
                route: GenerationAllocationRoute::ParentSyncFailed,
            }
        };
        expect(
            allocate_once_with(&cell, &transaction, allocate),
            42,
            GenerationAllocationRoute::ParentSyncFailed,
        );
        expect(
            allocate_once_with(&cell, &transaction, allocate),
            42,
            GenerationAllocationRoute::ParentSyncFailed,
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let production = include_str!("runtime_store.rs")
            .split_once("/// Production publishes")
            .unwrap()
            .1
            .split_once("struct GenerationFileLock")
            .unwrap()
            .0;
        let declaration = "static PROCESS_GENERATION: std::sync::OnceLock<";
        assert!(production.contains(declaration));
    }

    /// Bounded lexical architecture pin for the literal `cfg(not(test))` reader block.
    /// It catches reconstruction or field detachment inside that block, but does not claim
    /// semantic coverage for aliases, macros, or a reader moved outside these source bounds.
    #[test]
    fn production_binding_lexically_returns_the_stored_allocation_whole() {
        let source = include_str!("runtime_store.rs");
        let reader = source
            .split_once(
                "pub(in crate::services::discord) fn process_generation_binding() -> ProcessGenerationAllocation {",
            )
            .expect("process generation binding reader must remain present")
            .1
            .split_once("/// Preview the epoch that the replacement process will allocate")
            .expect("bounded reader end must remain present")
            .0;
        let production_read = reader
            .split_once("#[cfg(test)]")
            .expect("production and test reader branches must remain separate")
            .0;
        assert_eq!(
            production_read,
            "\n    #[cfg(not(test))]\n    if let Some(bound) = PROCESS_GENERATION.get() {\n        return *bound;\n    }\n    ",
            "the complete production reader prefix must exactly return the canonical whole allocation"
        );

        let process_reader = source
            .split_once("pub fn process_generation() -> u64 {")
            .expect("process generation accessor must remain present")
            .1
            .split_once(
                "pub(in crate::services::discord) fn process_generation_binding() -> ProcessGenerationAllocation {",
            )
            .expect("process generation accessor must remain bounded by the binding reader")
            .0
            .split_once("#[cfg(not(test))]")
            .expect("process generation production branch must remain present")
            .1
            .split_once("#[cfg(test)]")
            .expect("process generation production and test branches must remain separate")
            .0;
        assert_eq!(
            process_reader.trim(),
            "{\n        process_generation_binding().generation\n    }",
            "production row writers must return the published binding generation directly"
        );

        let allocator = source
            .split_once("pub(in crate::services::discord) fn allocate_process_generation_binding()")
            .expect("process generation allocator must remain present")
            .1
            .split_once("fn allocate_generation_epoch() -> ProcessGenerationAllocation {")
            .expect("process generation allocator must remain bounded by epoch allocation")
            .0
            .split_once("#[cfg(not(test))]")
            .expect("production allocator branch must remain present")
            .1
            .split_once("#[cfg(test)]")
            .expect("production and test allocator branches must remain separate")
            .0;
        assert_eq!(
            allocator.trim(),
            "{\n        allocate_once_with(\n            &PROCESS_GENERATION,\n            &PROCESS_GENERATION_ALLOCATION_TRANSACTION,\n            allocate_generation_epoch,\n        )\n    }",
            "the production allocator must exactly return the canonical whole-allocation publication"
        );

        let composer = source
            .split_once("fn allocate_generation_epoch() -> ProcessGenerationAllocation {")
            .expect("production allocation composer must remain present")
            .1
            .split_once("fn read_generation_counter(path: &Path) -> CounterRead {")
            .expect("production allocation composer must remain bounded")
            .0;
        assert_eq!(
            composer.trim(),
            "allocate_generation_epoch_with_io(GenerationIo {\n        path: generation_path(),\n        lock: lock_generation_path,\n        read: read_generation_counter,\n        write: atomic_write,\n        fsync: fsync_parent_dir,\n    })\n}",
            "the production composer must be exactly the canonical allocation tail expression"
        );
    }

    #[test]
    fn test_pin_memo_fallback_and_reset_remain_structurally_separate() {
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let path = generation_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        set_process_generation_for_tests(None);

        std::fs::write(&path, "7").unwrap();
        expect(
            allocate_process_generation_binding(),
            8,
            GenerationAllocationRoute::AdvancedWithSyncedRename,
        );
        std::fs::write(&path, "19").unwrap();
        expect(
            process_generation_binding(),
            19,
            GenerationAllocationRoute::Unwitnessed,
        );
        assert_eq!(process_generation(), 19);

        set_process_generation_for_tests(Some(73));
        expect(
            allocate_process_generation_binding(),
            73,
            GenerationAllocationRoute::Unwitnessed,
        );
        expect(
            process_generation_binding(),
            73,
            GenerationAllocationRoute::Unwitnessed,
        );

        set_process_generation_for_tests(None);
        std::fs::write(&path, "40").unwrap();
        expect(
            process_generation_binding(),
            40,
            GenerationAllocationRoute::Unwitnessed,
        );
        expect(
            allocate_process_generation_binding(),
            41,
            GenerationAllocationRoute::AdvancedWithSyncedRename,
        );
        set_process_generation_for_tests(None);
    }

    #[test]
    fn production_composer_advances_with_synced_parent() {
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let path = root.path().join("runtime").join("generation");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "7").unwrap();

        expect(
            allocate_generation_epoch(),
            8,
            GenerationAllocationRoute::AdvancedWithSyncedRename,
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "8");

        let source = include_str!("runtime_store.rs");
        let composer = source
            .split_once("fn allocate_generation_epoch() -> ProcessGenerationAllocation {")
            .unwrap()
            .1
            .split_once("fn read_generation_counter")
            .unwrap()
            .0;
        for binding in [
            "path: generation_path()",
            "lock: lock_generation_path",
            "read: read_generation_counter",
            "write: atomic_write",
            "fsync: fsync_parent_dir",
        ] {
            assert_eq!(composer.matches(binding).count(), 1, "binding={binding}");
        }
    }

    #[test]
    fn counter_read_failure_preserves_copy_detail() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            read_generation_counter(&root.path().join("missing")),
            CounterRead::Absent
        );
        let malformed = root.path().join("malformed");
        std::fs::write(&malformed, "nan").unwrap();
        assert_eq!(
            read_generation_counter(&malformed),
            CounterRead::Failed(CounterReadFailure::Parse)
        );
        let directory = root.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        assert!(matches!(read_generation_counter(&directory),
            CounterRead::Failed(CounterReadFailure::Io(kind))
                if kind != std::io::ErrorKind::NotFound));
    }

    fn capture_site_a(run: impl FnOnce()) -> String {
        let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(CapturingWriter(buffer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert_eq!(logs.lines().count(), 1, "logs={logs}");
        logs
    }

    #[test]
    fn site_a_emits_one_detailed_record_for_counter_read_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("generation");
        std::fs::write(&path, "nan").unwrap();
        let record = capture_site_a(|| {
            allocate_generation_epoch_with_io(io(path, |_| Ok(())));
        });
        for field in [
            "counter_read=\"parse_error\"",
            "counter_detail=\"parse\"",
            "route=\"counter_read_failed\"",
            "epoch_advanced=false",
        ] {
            assert!(record.contains(field), "{field}: {record}");
        }
    }

    #[test]
    fn real_lock_failure_and_synced_advance_are_reachable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock").join("generation");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "7").unwrap();
        std::fs::create_dir(path.with_extension("lock")).unwrap();
        expect(
            allocate_generation_epoch_with_io(io(path, |_| Ok(()))),
            7,
            GenerationAllocationRoute::LockFailed,
        );
        #[cfg(unix)]
        {
            let path = root.path().join("advanced").join("generation");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "7").unwrap();
            expect(
                allocate_generation_epoch_with_io(io(path, fsync_parent_dir)),
                8,
                GenerationAllocationRoute::AdvancedWithSyncedRename,
            );
        }
    }

    /// Lexical tripwire only: synonyms, moved text, and rewrites remain review work.
    #[test]
    fn epoch_provenance_surface_makes_no_survival_claim() {
        let source = include_str!("runtime_store.rs");
        let surface = source
            .split_once("// <epoch-provenance-surface>")
            .unwrap()
            .1
            .split_once("// </epoch-provenance-surface>")
            .unwrap()
            .0
            .to_ascii_lowercase();
        for word in ["durable", "persistent", "persisted", "survive", "crash"] {
            assert!(!surface.contains(word), "forbidden={word}");
        }
    }
}

#[cfg(test)]
mod runtime_root_tests {
    use super::*;

    #[test]
    fn live_release_override_falls_back_to_isolated_tempdir() {
        let home = dirs::home_dir().expect("test requires a home directory");
        let live_release_root = home.join(".adk").join("release");
        let _env =
            crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &live_release_root);

        let resolved = test_agentdesk_root().expect("test runtime root");

        assert_ne!(resolved, live_release_root);
        assert!(resolved.exists(), "fallback tempdir must remain alive");
    }

    #[test]
    fn process_generation_allocation_is_monotonic_and_request_preview_is_read_only() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());

        assert_eq!(allocate_process_generation(), 1);
        assert_eq!(next_process_generation(), 2);
        assert_eq!(load_generation(), 1);
        assert_eq!(allocate_process_generation(), 1);
        assert_eq!(load_generation(), 1);
        set_process_generation_for_tests(None);
    }

    #[test]
    fn concurrent_provider_allocations_share_one_epoch() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                allocate_process_generation()
            }));
        }
        let generations: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("allocator thread"))
            .collect();
        assert!(generations.iter().all(|generation| *generation == 1));
        assert_eq!(load_generation(), 1);
        set_process_generation_for_tests(None);
    }

    #[test]
    fn process_epoch_does_not_follow_overlapping_process_counter_advance() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        set_process_generation_for_tests(Some(7));
        let path = generation_path().expect("generation path");
        std::fs::create_dir_all(path.parent().expect("generation parent")).unwrap();
        atomic_write(&path, "8").unwrap();
        assert_eq!(load_generation(), 8);
        assert_eq!(process_generation(), 7);
        set_process_generation_for_tests(None);
    }
}

#[cfg(test)]
mod parent_dir_fsync_tests {
    use super::*;

    /// #5254 D10 [measured P8/P10]: the directory fsync succeeds on a real
    /// published file. The open mode is contract, not accident — opening a
    /// directory for writing fails with `EISDIR`, so a "make it writable"
    /// refactor of this helper turns a durability call into a permanent error.
    #[test]
    fn published_file_parent_dir_is_fsyncable() {
        let root = tempfile::tempdir().expect("runtime root");
        let published = root.path().join("restart_persisted.nonce-a");
        atomic_write(&published, "nonce=nonce-a\n").expect("publish");

        fsync_parent_dir(&published).expect("directory entry fsync");
    }

    /// The caller gates the derived index on this result, which is only safe if
    /// failure is reported rather than raised: this must be an `Err`, never a
    /// panic.
    #[test]
    fn missing_parent_dir_is_reported_not_panicked() {
        let root = tempfile::tempdir().expect("runtime root");
        let orphan = root.path().join("gone").join("restart_persisted.nonce-b");

        let error = fsync_parent_dir(&orphan).expect_err("absent parent directory");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

#[cfg(test)]
mod atomic_write_logging_tests {
    use super::*;
    use std::fs;

    #[test]
    fn critical_atomic_write_returns_error_for_unwritable_parent_path() {
        let tmp = tempfile::tempdir().unwrap();
        let parent_file = tmp.path().join("not-a-dir");
        fs::write(&parent_file, "blocking-file").unwrap();
        let target = parent_file.join("queue.json");

        let error = critical_atomic_write(
            &target,
            "[]",
            AtomicWriteContext::new("discord_pending_queue")
                .provider("codex")
                .token_hash("discord_deadbeef")
                .channel_id(42),
        )
        .expect_err("critical write must expose persistence failure");

        assert!(
            error.contains("create_dir_all") || error.contains("Not a directory"),
            "unexpected critical write error: {error}"
        );
    }
}
