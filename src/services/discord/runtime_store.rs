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

/// Load the current generation counter (returns 0 if file missing/corrupt).
pub fn load_generation() -> u64 {
    generation_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Increment the generation counter and return the new value.
pub fn increment_generation() -> u64 {
    let current = load_generation();
    let next = current + 1;
    if let Some(path) = generation_path() {
        best_effort_atomic_write_logged(
            &path,
            &next.to_string(),
            AtomicWriteContext::new("runtime_generation"),
        );
    }
    next
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
    tracing::info!(
        target: "agentdesk::inflight_remove",
        provider = %provider,
        channel_id = discord_inflight_atomic_replace_channel_id(path),
        user_msg_id = discord_inflight_atomic_replace_user_msg_id(path),
        reason = "runtime_store_atomic_write_replace",
        path = %path.display(),
        "discord inflight state row removal"
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
