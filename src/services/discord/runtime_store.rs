#[cfg(all(test, not(windows)))]
pub(crate) use crate::domain::file_sync::parent_sync_probe;
pub(crate) use crate::domain::file_sync::{PARENT_DIR_FSYNC_FLUSHES, fsync_parent_dir};

use std::fs;
use std::path::{Path, PathBuf};

mod durable_ancestors;
pub(crate) mod durable_publish;
pub(crate) use durable_publish::{
    best_effort_publish_logged, critical_publish, fsync_parent_and_ancestors, sync_removed,
};
#[cfg(test)]
pub(crate) use durable_publish::{critical_atomic_write, flush_probe as durable_probe};

pub(crate) use crate::services::runtime_paths::{agentdesk_root, runtime_root};

pub(super) fn worktrees_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("worktrees"))
}

pub(super) fn bot_settings_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::config_dir(&root).join("bot_settings.json"))
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

#[cfg(test)]
pub(super) fn discord_restart_reports_root() -> Option<PathBuf> {
    crate::services::runtime_paths::restart_reports_root()
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

#[cfg(test)]
pub(crate) use crate::services::process_generation::{
    ADVANCED_ROUTE_FOR_TESTS, GenerationAllocationRoute,
    allocate_and_publish_process_generation_for_tests, process_generation_allocation_for_tests,
    publish_process_generation_allocation_for_tests,
};
pub(crate) use crate::services::process_generation::{
    ProcessGenerationAllocation, process_generation_binding,
};
pub use crate::services::process_generation::{allocate_process_generation, process_generation};
#[cfg(test)]
pub use crate::services::process_generation::{
    generation_path, load_generation, next_process_generation, set_process_generation_for_tests,
};

pub(super) fn last_message_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("last_message"))
}

struct LastMessageIdFileLock {
    _file: fs::File,
}

impl Drop for LastMessageIdFileLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
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
    file.lock().map_err(|error| error.to_string())?;
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
    #[cfg(test)]
    checkpoint_lock_tests::after_read();
    best_effort_publish_logged(
        &path,
        &checkpoint.to_string(),
        AtomicWriteContext::new("last_message")
            .provider(provider)
            .channel_id(channel_id),
    );
}

#[cfg(test)]
#[path = "runtime_store_checkpoint_lock_tests.rs"]
mod checkpoint_lock_tests;

/// Save all last_message_ids from a map (used during SIGTERM).
pub(super) fn save_all_last_message_ids(provider: &str, ids: &std::collections::HashMap<u64, u64>) {
    for (channel_id, message_id) in ids {
        save_last_message_id(provider, *channel_id, *message_id);
    }
}

use crate::services::durable_publish::classify_io_error;

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
    use crate::domain::atomic_write::{Published, prepare};
    let failed = |failure: crate::domain::atomic_write::NotPublished| {
        classify_io_error(failure.stage.prefix(), failure.error)
    };
    let prepared = prepare(path, data).map_err(failed)?;
    log_discord_inflight_atomic_replace(path);
    prepared.publish().map(|Published| ()).map_err(failed)
}

#[cfg(test)]
pub(crate) use crate::domain::atomic_write::rename_fault;

/// (device, inode) of a directory, so a replaced directory is told from the one it replaced.
#[cfg(unix)]
fn dir_identity(meta: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Never consulted without a directory flush, which this platform lacks.
#[cfg(not(unix))]
fn dir_identity(_meta: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AtomicWriteContext<'a> {
    pub(crate) store: &'a str,
    pub(crate) provider: Option<&'a str>,
    pub(crate) token_hash: Option<&'a str>,
    pub(crate) channel_id: Option<u64>,
    pub(crate) session_key: Option<&'a str>,
    pub(crate) turn_id: Option<&'a str>,
    /// Trusted root of a durable publication; `None` means the runtime root.
    pub(crate) durable_root: Option<&'a Path>,
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
            durable_root: None,
        }
    }

    pub(crate) fn durable_root(mut self, root: &'a Path) -> Self {
        self.durable_root = Some(root);
        self
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

#[cfg(test)]
use crate::services::runtime_paths::test_agentdesk_root;

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
    #[cfg(not(windows))]
    #[test]
    fn missing_parent_dir_is_reported_not_panicked() {
        let root = tempfile::tempdir().expect("runtime root");
        let orphan = root.path().join("gone").join("restart_persisted.nonce-b");

        let error = fsync_parent_dir(&orphan).expect_err("absent parent directory");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

/// Its own `tests` family so the Windows PR lane can run exactly this set.
#[cfg(test)]
mod windows_contract {
    #[cfg(windows)]
    mod tests {
        use super::super::*;

        /// Windows `File::open` cannot open a directory, so a directory open
        /// here would fail every caller; the helper must return `Ok` unflushed.
        #[test]
        fn windows_parent_dir_sync_succeeds_without_flushing() {
            let root = tempfile::tempdir().expect("runtime root");
            let published = root.path().join("restart_persisted.nonce-w");
            atomic_write(&published, "nonce=nonce-w\n").expect("publish");

            fsync_parent_dir(&published).expect("Windows parent directory sync");
            assert!(!PARENT_DIR_FSYNC_FLUSHES);
        }
    }
}

#[cfg(test)]
#[path = "runtime_store_atomic_write_tests.rs"]
mod atomic_write_logging_tests;
