use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::services::tmux_diagnostics::clear_tmux_exit_reason;

const CLAUDE_TUI_READY_SCAN_LINES: usize = 12;
const CLAUDE_TUI_ACTIVE_SCAN_LINES: usize = 24;
const CLAUDE_TUI_DRAFT_SCAN_LINES: usize = 36;
const CLAUDE_TUI_READY_BANNER: &str = "Ready for input (type message + Enter)";
const CLAUDE_TUI_PROMPT_MARKER: &str = "\u{276f}";

fn trim_prompt_line(line: &str) -> &str {
    line.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}')
}

pub(crate) fn tmux_line_is_claude_tui_ready_prompt(line: &str) -> bool {
    trim_prompt_line(line) == CLAUDE_TUI_PROMPT_MARKER
}

fn tmux_line_is_claude_tui_prompt_draft(line: &str) -> bool {
    let Some(rest) = trim_prompt_line(line).strip_prefix(CLAUDE_TUI_PROMPT_MARKER) else {
        return false;
    };
    let rest = rest.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    // AgentDesk injects submitted Discord turns as lines like
    // `❯ [User: name (ID: ...)] ...`. Those are pane history, not an active
    // composer draft, so do not block the transcript-idle readiness fallback.
    let discord_submitted_prompt = rest
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("[User:"));
    !rest.is_empty() && !discord_submitted_prompt
}

fn tmux_lines_after_claude_prompt_show_completed_history(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let line = trim_prompt_line(line);
        let nonzero_tool_summary =
            line.contains("Tools:") && line.contains(" done") && !line.contains("Tools: 0 done");
        line.starts_with('⏺')
            || line.starts_with("✻ ")
            || line.contains("Baked for")
            || line.contains("Brewed for")
            || line.contains("Crunched for")
            || line.contains("Cogitated for")
            || nonzero_tool_summary
    })
}

fn tmux_lines_after_claude_prompt_show_idle_suggestion_chrome(lines: &[&str]) -> bool {
    let busy = lines.iter().any(|line| {
        let lower = trim_prompt_line(line).to_ascii_lowercase();
        lower.contains("esc to interrupt")
            || lower.contains("processing")
            || lower.contains("thinking")
            || lower.contains("running")
    });
    if busy {
        return false;
    }
    let separator = lines.iter().any(|line| {
        trim_prompt_line(line)
            .chars()
            .filter(|ch| *ch == '─')
            .count()
            >= 8
    });
    let idle_footer = lines.iter().any(|line| {
        let line = trim_prompt_line(line);
        line.contains("Tools: 0 done") || line.contains("bypass permissions")
    });
    separator && idle_footer
}

pub(crate) fn tmux_capture_indicates_claude_tui_ready_for_input(capture: &str) -> bool {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_ACTIVE_SCAN_LINES);
    let recent_forward = &non_empty[start..];
    let recent = recent_forward.iter().rev().copied().collect::<Vec<_>>();

    if recent.iter().any(|l| l.contains(CLAUDE_TUI_READY_BANNER)) {
        return true;
    }
    if tmux_recent_lines_show_claude_tui_active_work(&recent) {
        return false;
    }

    if recent
        .iter()
        .take(CLAUDE_TUI_READY_SCAN_LINES)
        .any(|l| tmux_line_is_claude_tui_ready_prompt(l))
    {
        return true;
    }

    recent_forward
        .iter()
        .enumerate()
        .rev()
        .take(CLAUDE_TUI_READY_SCAN_LINES)
        .any(|(index, line)| {
            if !tmux_line_is_claude_tui_prompt_draft(line) {
                return false;
            }
            let after_prompt = &recent_forward[index + 1..];
            tmux_lines_after_claude_prompt_show_completed_history(after_prompt)
                || tmux_lines_after_claude_prompt_show_idle_suggestion_chrome(after_prompt)
        })
}

pub(crate) fn tmux_capture_indicates_claude_tui_prompt_draft(capture: &str) -> bool {
    tmux_capture_claude_tui_prompt_draft_backspace_budget(capture).is_some()
}

pub(crate) fn tmux_capture_indicates_claude_tui_idle_suggestion(capture: &str) -> bool {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_DRAFT_SCAN_LINES);
    let recent = &non_empty[start..];
    recent
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, line)| {
            if !trim_prompt_line(line).starts_with(CLAUDE_TUI_PROMPT_MARKER) {
                return None;
            }
            if !tmux_line_is_claude_tui_prompt_draft(line) {
                return Some(false);
            }
            let after_prompt = &recent[index + 1..];
            if tmux_lines_after_claude_prompt_show_completed_history(after_prompt) {
                return Some(false);
            }
            Some(tmux_lines_after_claude_prompt_show_idle_suggestion_chrome(
                after_prompt,
            ))
        })
        .unwrap_or(false)
}

fn tmux_recent_lines_show_claude_tui_active_work(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let line = trim_prompt_line(line);
        let lower = line.to_ascii_lowercase();
        line.contains("Actioning")
            || line.contains("Musing")
            || lower.contains("esc to interrupt")
            || lower.contains("current work")
            || (line.starts_with('⏺')
                && ((line.contains("Running ") && line.contains("command"))
                    || line.contains("Searching for ")
                    || line.contains("Reading ")
                    || line.contains("Editing ")))
    })
}

pub(crate) fn tmux_capture_claude_tui_prompt_draft_backspace_budget(
    capture: &str,
) -> Option<usize> {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_DRAFT_SCAN_LINES);
    let recent = &non_empty[start..];
    recent
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, line)| {
            if !trim_prompt_line(line).starts_with(CLAUDE_TUI_PROMPT_MARKER) {
                return None;
            }
            if !tmux_line_is_claude_tui_prompt_draft(line) {
                return Some(None);
            }
            // Claude keeps submitted prompt lines in the pane history. If the
            // prompt line is followed by rendered assistant/completion output,
            // it is historical text, not an editable composer draft.
            if tmux_lines_after_claude_prompt_show_completed_history(&recent[index + 1..]) {
                return Some(None);
            }
            Some(claude_tui_prompt_draft_backspace_budget_from_line(line))
        })
        .unwrap_or(None)
}

pub(crate) fn claude_tui_prompt_draft_backspace_budget_from_line(line: &str) -> Option<usize> {
    let rest = trim_prompt_line(line)
        .strip_prefix(CLAUDE_TUI_PROMPT_MARKER)?
        .trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    if rest.is_empty()
        || rest
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("[User:"))
    {
        return None;
    }
    Some(rest.chars().count().saturating_add(4).min(512))
}

pub(crate) fn tmux_capture_indicates_generic_ready_banner(capture: &str) -> bool {
    capture
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(CLAUDE_TUI_READY_SCAN_LINES)
        .any(|l| l.contains(CLAUDE_TUI_READY_BANNER))
}

/// Format a tmux session name as an exact-match target.
///
/// tmux `-t` flags perform prefix matching by default: `-t foo` matches
/// both `foo` and `foo-bar`.  Prefixing with `=` forces exact matching,
/// preventing the wrong session from being targeted when session names
/// share a common prefix (e.g. main vs thread sessions).
pub fn tmux_exact_target(session_name: &str) -> String {
    format!("={}", session_name)
}

/// Subdirectory under the runtime root where session temp files live.
const SESSIONS_SUBDIR: &str = "runtime/sessions";
pub(crate) const CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT: &str = "claude-tui-settings.json";
pub(crate) const CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT: &str = "claude-tui.sh";
pub(crate) const CODEX_TUI_HOME_TEMP_EXT: &str = "codex-tui-home";
pub(crate) const TMUX_DEAD_MARKER_TEMP_EXT: &str = "pane_dead";
pub(crate) const TMUX_RUNTIME_KIND_TEMP_EXT: &str = "runtime-kind";

/// Returns the persistent AgentDesk sessions directory, if a runtime root
/// is configured. This is the new canonical location for session temp files
/// (jsonl, input FIFO, owner markers, prompt, etc.).
///
/// Returns None when `runtime_root()` is unavailable (rare; only during
/// very early bootstrap or broken environments). Callers should fall back
/// to `std::env::temp_dir()` in that case — see `agentdesk_temp_dir()`.
pub fn persistent_sessions_dir() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| root.join(SESSIONS_SUBDIR))
}

/// Get the platform-appropriate directory for AgentDesk session runtime files.
///
/// Prefers the persistent path under `runtime_root()/runtime/sessions/` so
/// that session jsonl/FIFO/owner markers survive across dcserver restarts
/// (see issue #892). Falls back to `std::env::temp_dir()` only when a
/// runtime root is not available.
pub fn agentdesk_temp_dir() -> String {
    match persistent_sessions_dir() {
        Some(dir) => {
            // Best-effort lazy create so early callers (tests, one-off tools)
            // don't fail before the dcserver startup bootstrap runs. The
            // startup code also calls `ensure_sessions_dir_on_startup()` so
            // wrappers spawned after boot write into the right place.
            let _ = ensure_sessions_dir_inner(&dir);
            dir.display().to_string()
        }
        None => std::env::temp_dir().display().to_string(),
    }
}

fn ensure_sessions_dir_inner(dir: &PathBuf) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(dir) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 != 0o700 {
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(dir, perms);
            }
        }
    }
    Ok(())
}

/// Startup hook: create the persistent sessions directory (0o700) so that
/// wrappers spawned after dcserver boot write into the canonical location.
/// Idempotent; safe to call multiple times.
pub fn ensure_sessions_dir_on_startup() -> Result<(), String> {
    let Some(dir) = persistent_sessions_dir() else {
        return Ok(()); // nothing to do when no runtime_root
    };
    ensure_sessions_dir_inner(&dir)
        .map_err(|e| format!("Failed to create sessions dir '{}': {}", dir.display(), e))
}

fn host_temp_namespace() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn session_temp_prefix(session_name: &str) -> String {
    let host = host_temp_namespace();
    let mut hasher = Sha256::new();
    hasher.update(current_tmux_owner_marker().as_bytes());
    hasher.update(b"|");
    hasher.update(host.as_bytes());
    let digest = hasher.finalize();
    let runtime_hash = format!("{:x}", digest);
    format!(
        "agentdesk-{}-{}-{}",
        &runtime_hash[..12],
        host,
        session_name
    )
}

/// Build a path for an AgentDesk runtime temp file in the **canonical**
/// (persistent) location.
///
/// Example: `session_temp_path("mySession", "jsonl")`
///   → `~/.adk/release/runtime/sessions/agentdesk-<runtime>-<host>-mySession.jsonl`
pub fn session_temp_path(session_name: &str, extension: &str) -> String {
    format!(
        "{}/{}.{}",
        agentdesk_temp_dir(),
        session_temp_prefix(session_name),
        extension
    )
}

/// Canonical marker written by tmux pane/session hooks when a session's pane
/// exits. Watchers treat this as an explicit "tmux died" wake-up; the legacy
/// liveness probe remains as a hook-miss safety net.
pub fn session_dead_marker_path(session_name: &str) -> String {
    session_temp_path(session_name, TMUX_DEAD_MARKER_TEMP_EXT)
}

/// Build a path to the *legacy* `/tmp/`-based location for a session temp
/// file. Wrappers spawned before the migration hold open fds to these files;
/// readers must be able to still find them during the migration window.
pub fn legacy_tmp_session_path(session_name: &str, extension: &str) -> String {
    format!(
        "{}/{}.{}",
        std::env::temp_dir().display(),
        session_temp_prefix(session_name),
        extension
    )
}

/// Resolve whichever location actually holds the session temp file.
/// Prefers the new persistent path when both exist. Returns `None` when
/// neither location has the file. Used by read-side code (e.g. the
/// `session_usable` check and the watcher skip-on-missing-output file)
/// so they accept either location during the migration window.
pub fn resolve_session_temp_path(session_name: &str, extension: &str) -> Option<String> {
    let new_path = session_temp_path(session_name, extension);
    if std::path::Path::new(&new_path).exists() {
        return Some(new_path);
    }
    let legacy = legacy_tmp_session_path(session_name, extension);
    if std::path::Path::new(&legacy).exists() {
        return Some(legacy);
    }
    None
}

/// Delete all known session temp files for the given tmux session.
/// Idempotent — missing files are not errors. Hits both the new persistent
/// location and the legacy `/tmp/` location so cleanup is total regardless
/// of where the wrapper originally wrote.
pub fn cleanup_session_temp_files(session_name: &str) {
    // All extensions we ever allocate under the session prefix.
    const EXTS: &[&str] = &[
        "jsonl",
        "input",
        "prompt",
        "owner",
        "sh",
        "generation",
        "exit_reason",
        TMUX_RUNTIME_KIND_TEMP_EXT,
        TMUX_DEAD_MARKER_TEMP_EXT,
        CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT,
        CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    ];
    for ext in EXTS {
        let _ = std::fs::remove_file(session_temp_path(session_name, ext));
        let _ = std::fs::remove_file(legacy_tmp_session_path(session_name, ext));
    }
    let _ = std::fs::remove_dir_all(session_temp_path(session_name, CODEX_TUI_HOME_TEMP_EXT));
    let _ = std::fs::remove_dir_all(legacy_tmp_session_path(
        session_name,
        CODEX_TUI_HOME_TEMP_EXT,
    ));
}

/// Get the current AgentDesk runtime root marker for tmux session ownership.
pub fn current_tmux_owner_marker() -> String {
    crate::config::runtime_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".adk/release".to_string())
}

/// Path to the owner marker file for a tmux session.
pub fn tmux_owner_path(tmux_session_name: &str) -> String {
    session_temp_path(tmux_session_name, "owner")
}

/// Write the owner marker file so this runtime claims the tmux session.
pub fn write_tmux_owner_marker(tmux_session_name: &str) -> Result<(), String> {
    clear_tmux_exit_reason(tmux_session_name);
    let owner_path = tmux_owner_path(tmux_session_name);
    std::fs::write(&owner_path, current_tmux_owner_marker())
        .map_err(|e| format!("Failed to write tmux owner marker: {}", e))
}

pub(crate) fn write_tmux_runtime_kind_marker(
    tmux_session_name: &str,
    runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind,
) -> Result<(), String> {
    let path = session_temp_path(tmux_session_name, TMUX_RUNTIME_KIND_TEMP_EXT);
    std::fs::write(&path, runtime_kind.as_str())
        .map_err(|e| format!("Failed to write tmux runtime kind marker: {}", e))
}

pub(crate) fn resolve_tmux_runtime_kind_marker(
    tmux_session_name: &str,
) -> Option<crate::services::agent_protocol::RuntimeHandoffKind> {
    let path = resolve_session_temp_path(tmux_session_name, TMUX_RUNTIME_KIND_TEMP_EXT)?;
    let raw = std::fs::read_to_string(path).ok()?;
    crate::services::agent_protocol::RuntimeHandoffKind::from_str(&raw)
}

/// Append-only JSONL writer that reopens the path when external rotation
/// replaces the file behind the path with a different inode.
#[derive(Debug)]
pub struct RotatingJsonlWriter {
    path: PathBuf,
    file: File,
}

impl RotatingJsonlWriter {
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let file = open_jsonl_append_file(&path)?;
        Ok(Self { path, file })
    }

    pub fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.reopen_if_path_replaced()?;
        writeln!(self.file, "{}", line)?;
        self.file.flush()
    }

    pub fn sync_all(&mut self) -> std::io::Result<()> {
        self.file.sync_all()
    }
    fn reopen_if_path_replaced(&mut self) -> std::io::Result<()> {
        if path_points_to_different_file(&self.file, &self.path)? {
            self.file = open_jsonl_append_file(&self.path)?;
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
impl RotatingJsonlWriter {
    #[cfg(unix)]
    fn bound_file_id(&self) -> std::io::Result<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;

        let meta = self.file.metadata()?;
        Ok((meta.dev(), meta.ino()))
    }
}

fn open_jsonl_append_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// #2442 — JSONL sentinel emitted by wrappers so the watcher /
/// recovery_engine can graduate the 2s drain quiet-period and 2s
/// ready-probe interval.
///
/// The wrapper writes one line per event directly to the session JSONL
/// using the same append-then-flush path as normal stream-json output.
/// Two flavors:
///  - `terminal_end` — emitted by `scopeguard` at wrapper exit (any exit
///    path the runtime can observe — clean exit, panic unwind). The
///    consumer treats this as a deterministic drain marker so the 2s
///    quiet-period in `recovery_engine.rs` can short-circuit. We still
///    keep the 2s fallback for SIGKILL paths that bypass scopeguard.
///  - `ready_for_input` — emitted by each wrapper immediately before/after
///    handing stdin off to the provider when the provider has signalled
///    readiness. The 2s probe-interval in `tmux.rs` short-circuits on
///    arrival; if the wrapper never writes (e.g. SIGKILL mid-turn) the
///    probe falls back to its existing cadence.
///
/// Both helpers are best-effort: a failure to write the sentinel never
/// affects the wrapper's primary work. Errors are silently dropped — the
/// 2s fallbacks on the consumer side keep behavior correct.
#[derive(Clone, Copy, Debug)]
pub enum WrapperSentinel<'a> {
    /// Wrapper is exiting. `exit` carries the runtime-derived reason
    /// string (`exit:N` / `signal:N` / `still_running`) for diagnostics.
    TerminalEnd { exit: &'a str },
    /// Provider has signalled readiness — wrapper is about to (or just
    /// did) accept further stdin. `provider` identifies the wrapper kind.
    ReadyForInput { provider: &'a str },
}

/// Public name of the JSONL `type` field for the terminal-end sentinel.
/// Exposed as a constant so consumers (recovery_engine.rs) and producers
/// (wrappers) can agree on the wire-level event name without string
/// duplication.
pub const WRAPPER_TERMINAL_END_EVENT: &str = "terminal_end";
/// Public name of the JSONL `type` field for the ready-for-input sentinel.
pub const WRAPPER_READY_FOR_INPUT_EVENT: &str = "ready_for_input";

/// Emit a sentinel line into the session JSONL. Best-effort; errors are
/// swallowed because the consumer-side fallbacks (2s drain quiet-period,
/// 2s ready-probe interval) keep behavior correct even when the sentinel
/// never lands.
pub fn emit_wrapper_sentinel(output_file: &str, sentinel: WrapperSentinel<'_>) {
    let line = match sentinel {
        WrapperSentinel::TerminalEnd { exit } => serde_json::json!({
            "type": WRAPPER_TERMINAL_END_EVENT,
            "exit": exit,
            "ts": chrono::Utc::now().to_rfc3339(),
        }),
        WrapperSentinel::ReadyForInput { provider } => serde_json::json!({
            "type": WRAPPER_READY_FOR_INPUT_EVENT,
            "provider": provider,
            "ts": chrono::Utc::now().to_rfc3339(),
        }),
    };
    let Ok(mut writer) = RotatingJsonlWriter::open(output_file) else {
        return;
    };
    let _ = writer.write_line(&line.to_string());
    let _ = writer.sync_all();
}

#[cfg(unix)]
fn path_points_to_different_file(file: &File, path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let file_meta = file.metadata()?;
    let path_meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(file_meta.dev() != path_meta.dev() || file_meta.ino() != path_meta.ino())
}

#[cfg(not(unix))]
fn path_points_to_different_file(_file: &File, _path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

// ── Rolling head-truncate for session jsonl ─────────────────────────────
//
// We cap session jsonl files at SIZE_CAP_BYTES. When they exceed the cap,
// we truncate from the head keeping ~TARGET_KEEP_BYTES worth of the most
// recent complete lines. A partial leading line after truncation is dropped
// so downstream stream-json parsers never see half of a record.

/// Soft cap at which we trigger head-truncation.
pub const JSONL_SIZE_CAP_BYTES: u64 = 20 * 1024 * 1024;
/// Target size to keep after truncation.
pub const JSONL_TARGET_KEEP_BYTES: u64 = 15 * 1024 * 1024;

/// Truncate a jsonl file from the head, keeping only complete lines totaling
/// at most `target_keep_bytes`. A leading partial line after the keep-window
/// is dropped so the first byte of the rewritten file is the first byte of a
/// complete line.
///
/// Returns `Ok(Some(new_size))` if the file was rewritten, `Ok(None)` if the
/// file is under cap or missing.
pub fn truncate_jsonl_head_safe(
    path: &str,
    size_cap_bytes: u64,
    target_keep_bytes: u64,
) -> std::io::Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let size = meta.len();
    if size <= size_cap_bytes {
        return Ok(None);
    }

    // Figure out the byte offset we *want* to start keeping from.
    let start_offset = size.saturating_sub(target_keep_bytes);

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;
    let mut buf = Vec::with_capacity((size - start_offset) as usize);
    file.read_to_end(&mut buf)?;
    drop(file);

    // Drop any partial leading line: advance past the first newline so the
    // kept buffer begins at a line boundary. If no newline exists in buf
    // at all, we're keeping a single partial line — drop everything rather
    // than risk emitting a garbled record. (This is the rare case where
    // target_keep_bytes lands in the middle of an exceptionally huge line.)
    let keep_start = if start_offset == 0 {
        0 // no truncation needed at the head
    } else {
        match buf.iter().position(|b| *b == b'\n') {
            Some(idx) => idx + 1,
            None => buf.len(), // nothing complete to keep
        }
    };

    let kept = &buf[keep_start..];
    let new_size = kept.len() as u64;

    // Atomic-ish rewrite: write to sibling temp then rename.
    let tmp_path = format!("{}.truncate.tmp", path);
    {
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        out.write_all(kept)?;
        out.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(Some(new_size))
}

#[cfg(test)]
mod sentinel_tests {
    use super::*;

    /// #2442 — round-trip the sentinel through the same code path the
    /// wrappers use, then verify the consumer-side tail-peek picks it up.
    #[test]
    fn emit_wrapper_sentinel_writes_terminal_end_line() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("session.jsonl");
        // Seed with normal output so the sentinel lands in the tail
        // window after some legit content.
        std::fs::write(&path, "{\"type\":\"assistant\",\"text\":\"hi\"}\n").unwrap();

        emit_wrapper_sentinel(
            path.to_str().unwrap(),
            WrapperSentinel::TerminalEnd { exit: "exit:0" },
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(&format!("\"type\":\"{}\"", WRAPPER_TERMINAL_END_EVENT)),
            "terminal_end sentinel must be present in the jsonl, got:\n{content}",
        );
        assert!(content.contains("\"exit\":\"exit:0\""));
    }

    /// #2442 — ready_for_input variant emits the correct provider tag so
    /// downstream consumers can attribute the readiness signal.
    #[test]
    fn emit_wrapper_sentinel_writes_ready_for_input_line() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("session.jsonl");

        emit_wrapper_sentinel(
            path.to_str().unwrap(),
            WrapperSentinel::ReadyForInput { provider: "codex" },
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(&format!("\"type\":\"{}\"", WRAPPER_READY_FOR_INPUT_EVENT)));
        assert!(content.contains("\"provider\":\"codex\""));
    }

    #[test]
    fn dead_marker_path_is_cleaned_with_session_temp_files() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");

        let tdir =
            std::env::temp_dir().join(format!("adk-issue-2424-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
            std::env::set_var("HOSTNAME", "issue-2424-host");
        }

        let session = format!("issue-2424-cleanup-sess-{}", std::process::id());
        let marker_path = session_dead_marker_path(&session);
        if let Some(parent) = std::path::Path::new(&marker_path).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&marker_path, "pane-exited").unwrap();

        cleanup_session_temp_files(&session);

        assert!(
            !std::path::Path::new(&marker_path).exists(),
            "cleanup_session_temp_files must remove pane-death marker: {marker_path}"
        );

        let _ = std::fs::remove_dir_all(&tdir);
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
    }

    #[test]
    fn claude_prompt_draft_detector_blocks_active_operator_draft() {
        let capture = "\
assistant output
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}operator is still typing
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%";

        assert!(tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_ready_prompt_rejects_active_work_chrome() {
        let capture = "\
⏺ Running 1 shell command…
· Actioning… (4m 7s · ↓ 9.4k tokens)
  ⎿  Tip: Use /btw to ask a quick side question without interrupting Claude's
     current work
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%
  CLAUDE.md: 1, MCP: 2 │ Tools: 12 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_ready_prompt_accepts_idle_empty_prompt() {
        let capture = "\
✻ Churned for 4m 56s
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%
  CLAUDE.md: 1, MCP: 2 │ Tools: 17 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_ready_prompt_accepts_submitted_prompt_with_idle_footer() {
        let capture = "\
✻ Crunched for 32s
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}claude-e 추가 채널 확장 진행해
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 5%
  CLAUDE.md: 1, MCP: 2 │ Tools: 4 done
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_ignores_submitted_discord_history_prompt() {
        let capture = "\
❯ [User: 0hbujang (ID: 343742347365974026)] 이전 턴
⏺ 처리했습니다.
✻ Baked for 2s
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_ignores_submitted_direct_history_prompt() {
        let capture = "\
❯ direct prompt typed through ssh
⏺ direct prompt typed through ssh
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_ignores_response_tail_with_tool_summary() {
        let capture = "\
❯ 계획만 적고 보류해줘
계획만 적고 보류 — 1개
  📁 claude-adk-cc-20260523-070547
  CLAUDE.md: 1, MCP: 2 │ Tools: 5 done";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert_eq!(
            tmux_capture_claude_tui_prompt_draft_backspace_budget(capture),
            None
        );
    }

    #[test]
    fn claude_prompt_draft_detector_uses_wider_window_for_history_completion() {
        let capture = "\
❯ direct prompt typed through ssh
  wrapped prompt line
  more wrapped prompt line
  filler 01
  filler 02
  filler 03
  filler 04
  filler 05
  filler 06
  filler 07
  filler 08
  filler 09
  filler 10
  filler 11
  filler 12
⏺ direct prompt typed through ssh
✻ Brewed for 2s";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_treats_running_submitted_prompt_as_not_ready() {
        let capture = "\
⏺ previous response
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
❯ direct prompt that has just been submitted
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done";

        assert!(tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_idle_suggestion_prompt_is_not_recoverable_draft_context() {
        let capture = "\
⏺ TUI-E2E marker
✻ Worked for 2s
────────────────────────────────────────────────────────────────────────────
❯\u{00a0}좋아, 잘 동작하네
────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ░░░░░░░░░░ │ 4%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(tmux_capture_indicates_claude_tui_idle_suggestion(capture));
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[test]
    fn session_temp_path_is_namespaced_by_runtime_root() {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");

        unsafe {
            std::env::set_var("HOSTNAME", "test-host");
            std::env::set_var("AGENTDESK_ROOT_DIR", "/tmp/adk-runtime-a");
        }
        let path_a = session_temp_path("tmux-a", "jsonl");

        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", "/tmp/adk-runtime-b") };
        let path_b = session_temp_path("tmux-a", "jsonl");

        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }

        assert_ne!(path_a, path_b);
        assert!(path_a.contains("tmux-a"));
        assert!(path_b.contains("tmux-a"));
    }

    #[test]
    fn session_temp_path_uses_persistent_runtime_dir_when_root_is_set() {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");

        // tmpdir we own for the test
        let tdir =
            std::env::temp_dir().join(format!("adk-issue-892-persistent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
        }

        let path = session_temp_path("tmux-persistent-test", "jsonl");
        let expected_prefix = tdir.join("runtime").join("sessions");
        assert!(
            path.starts_with(&expected_prefix.display().to_string()),
            "expected {} to start with {}",
            path,
            expected_prefix.display()
        );

        // agentdesk_temp_dir() should have created the directory as a side
        // effect — verify it's there and accessible.
        assert!(
            expected_prefix.exists(),
            "persistent sessions dir not created"
        );

        // Restore env and clean up.
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        let _ = std::fs::remove_dir_all(&tdir);
    }

    #[test]
    fn agentdesk_temp_dir_uses_persistent_sessions_subpath() {
        // Verify that when runtime_root() is Some(root), agentdesk_temp_dir()
        // returns a path ending in `runtime/sessions`. We don't clear HOME in
        // this test because other concurrent tests rely on env stability —
        // instead we assert the structural property.
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");

        let tdir =
            std::env::temp_dir().join(format!("adk-issue-892-subpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
        }

        let dir = agentdesk_temp_dir();
        let expected = tdir.join("runtime").join("sessions");
        assert_eq!(dir, expected.display().to_string());

        // Fallback branch: when persistent_sessions_dir() is None
        // (no runtime_root available) we must return std::env::temp_dir().
        // We can't easily force runtime_root()→None without clobbering HOME
        // for concurrent tests, so we test the inner decision explicitly
        // by asserting persistent_sessions_dir is Some(expected) — its
        // presence exercises the Some arm; the None arm is trivially
        // `std::env::temp_dir().display().to_string()`.
        assert_eq!(persistent_sessions_dir(), Some(expected));

        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        let _ = std::fs::remove_dir_all(&tdir);
    }

    #[test]
    fn resolve_session_temp_path_prefers_new_over_legacy() {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");

        let tdir =
            std::env::temp_dir().join(format!("adk-issue-892-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
            std::env::set_var("HOSTNAME", "resolve-host");
        }

        let session = format!("issue-892-resolve-sess-{}", std::process::id());

        // No files anywhere → None.
        assert!(
            resolve_session_temp_path(&session, "jsonl").is_none(),
            "expected None when neither location has the file"
        );

        // Create the legacy file only.
        let legacy = legacy_tmp_session_path(&session, "jsonl");
        std::fs::write(&legacy, b"legacy").unwrap();
        assert_eq!(
            resolve_session_temp_path(&session, "jsonl"),
            Some(legacy.clone()),
            "expected legacy path when only legacy exists"
        );

        // Create the new persistent file — should win.
        let new_path = session_temp_path(&session, "jsonl");
        std::fs::create_dir_all(std::path::Path::new(&new_path).parent().unwrap()).unwrap();
        std::fs::write(&new_path, b"new").unwrap();
        assert_eq!(
            resolve_session_temp_path(&session, "jsonl"),
            Some(new_path.clone()),
            "expected new path to be preferred over legacy"
        );

        // Cleanup.
        let _ = std::fs::remove_file(&legacy);
        let _ = std::fs::remove_file(&new_path);
        let _ = std::fs::remove_dir_all(&tdir);

        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
    }

    #[test]
    fn truncate_jsonl_head_safe_drops_partial_leading_line() {
        let tdir = std::env::temp_dir().join(format!("adk-issue-892-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&tdir).unwrap();
        let path = tdir.join("session.jsonl");

        // Build a file: several known-length lines, each ending in \n.
        // Each line: "line-NN:<pad>\n" — 100 bytes total so it's easy to reason about.
        let line_size = 100usize;
        let lines: Vec<String> = (0..200)
            .map(|i| {
                let prefix = format!("line-{:03}:", i);
                let pad = line_size - prefix.len() - 1; // -1 for \n
                format!("{}{}\n", prefix, "x".repeat(pad))
            })
            .collect();
        let content: String = lines.concat();
        std::fs::write(&path, &content).unwrap();

        // Cap at 5 KB, keep ~3.5 KB → must preserve a whole number of lines
        // ending with the last line of input.
        let cap = 5_000u64;
        let keep = 3_500u64;
        let result =
            truncate_jsonl_head_safe(path.to_str().unwrap(), cap, keep).expect("truncate ok");
        assert!(result.is_some(), "file should have been truncated");

        let after = std::fs::read_to_string(&path).unwrap();

        // 1. Every kept line must be complete (file ends with \n).
        assert!(
            after.ends_with('\n'),
            "truncated file must end with newline"
        );

        // 2. Last line of output equals last line of input.
        let last_out = after.lines().last().unwrap();
        let last_in = lines.last().unwrap().trim_end_matches('\n');
        assert_eq!(
            last_out, last_in,
            "last kept line should be last input line"
        );

        // 3. No partial first line. Every output line must match a whole input line.
        for out_line in after.lines() {
            assert!(
                lines.iter().any(|l| l.trim_end_matches('\n') == out_line),
                "unexpected partial line in output: {out_line}"
            );
        }

        // 4. Size is within the keep target (give or take one whole line).
        let new_size = after.len() as u64;
        assert!(
            new_size <= keep,
            "new size {} should be <= target keep {}",
            new_size,
            keep
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(&tdir);
    }

    #[test]
    fn truncate_jsonl_head_safe_no_op_under_cap() {
        let tdir =
            std::env::temp_dir().join(format!("adk-issue-892-trunc-noop-{}", std::process::id()));
        std::fs::create_dir_all(&tdir).unwrap();
        let path = tdir.join("small.jsonl");
        std::fs::write(&path, b"line1\nline2\n").unwrap();
        let result = truncate_jsonl_head_safe(path.to_str().unwrap(), 1_000_000, 500_000)
            .expect("truncate ok");
        assert!(result.is_none(), "small file should not be truncated");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line1\nline2\n");
        let _ = std::fs::remove_dir_all(&tdir);
    }

    #[test]
    fn truncate_jsonl_head_safe_missing_file_returns_none() {
        let result =
            truncate_jsonl_head_safe("/tmp/issue-892-does-not-exist-xyz.jsonl", 1_000, 500)
                .expect("missing file should be ok");
        assert!(result.is_none());
    }

    #[test]
    fn rotating_jsonl_writer_reopens_after_path_replacement() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("session.jsonl");
        let mut writer = RotatingJsonlWriter::open(&path).expect("open writer");

        writer
            .write_line(r#"{"type":"assistant","message":"before"}"#)
            .unwrap();

        let replacement = path.with_extension("jsonl.truncate.tmp");
        std::fs::write(
            &replacement,
            "{\"type\":\"assistant\",\"message\":\"kept\"}\n",
        )
        .unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        writer
            .write_line(r#"{"type":"assistant","message":"after"}"#)
            .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(r#"{"type":"assistant","message":"kept"}"#),
            "replacement content must survive rotation: {content}"
        );
        assert!(
            content.contains(r#"{"type":"assistant","message":"after"}"#),
            "writer must reopen and append to the replacement path: {content}"
        );
        assert!(
            !content.contains(r#"{"type":"assistant","message":"before"}"#),
            "replaced path should not retain pre-rotation content in this fixture: {content}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rotating_jsonl_writer_syncs_old_fd_before_reopening_after_rotation() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("session.jsonl");
        let stale = tdir.path().join("session.stale.jsonl");
        let mut writer = RotatingJsonlWriter::open(&path).expect("open writer");

        writer
            .write_line(r#"{"type":"assistant","message":"before"}"#)
            .expect("write before");
        let old_id = writer.bound_file_id().expect("old file id");

        std::fs::rename(&path, &stale).expect("move old file aside");
        std::fs::write(&path, "{\"type\":\"assistant\",\"message\":\"kept\"}\n")
            .expect("write replacement");

        writer.sync_all().expect("sync old fd");
        assert_eq!(
            writer.bound_file_id().expect("bound file id after sync"),
            old_id,
            "sync_all must fsync the original fd before any later reopen"
        );

        writer
            .write_line(r#"{"type":"assistant","message":"after"}"#)
            .expect("write after");

        let replacement = std::fs::read_to_string(&path).expect("read replacement");
        assert!(replacement.contains(r#"{"type":"assistant","message":"kept"}"#));
        assert!(replacement.contains(r#"{"type":"assistant","message":"after"}"#));

        let stale_content = std::fs::read_to_string(&stale).expect("read stale");
        assert!(stale_content.contains(r#"{"type":"assistant","message":"before"}"#));
    }

    // #1261 (Fix B): the watcher cleanup writes a `death_pane_log` snapshot
    // before killing the dead-pane session so the wrapper-level stderr that
    // never reached the structured jsonl is still recoverable post-mortem.
    // `cleanup_session_temp_files` MUST NOT delete this snapshot — otherwise
    // it would be erased microseconds after being written.
    //
    // The deletable EXTS list is the cleanup contract; pin its shape here so
    // a future "let's also nuke death_pane_log" tweak fails this test instead
    // of silently re-breaking post-mortem.
    #[test]
    fn cleanup_session_temp_files_preserves_death_pane_log_snapshot() {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");

        let tdir =
            std::env::temp_dir().join(format!("adk-issue-1261-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
            std::env::set_var("HOSTNAME", "issue-1261-host");
        }

        let session = format!("issue-1261-cleanup-sess-{}", std::process::id());

        // Seed every cleanup-eligible extension *plus* the death_pane_log
        // snapshot Fix B writes.
        let cleaned_exts = [
            "jsonl",
            "input",
            "prompt",
            "owner",
            "sh",
            "generation",
            "exit_reason",
            TMUX_RUNTIME_KIND_TEMP_EXT,
            TMUX_DEAD_MARKER_TEMP_EXT,
            CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT,
            CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        ];
        let mut cleaned_paths = Vec::new();
        for ext in &cleaned_exts {
            let path = session_temp_path(&session, ext);
            if let Some(parent) = std::path::Path::new(&path).parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, format!("seed-{ext}")).unwrap();
            cleaned_paths.push(path);
        }
        let death_log_path = session_temp_path(&session, "death_pane_log");
        std::fs::write(&death_log_path, "post-mortem capture").unwrap();

        cleanup_session_temp_files(&session);

        for path in &cleaned_paths {
            assert!(
                !std::path::Path::new(path).exists(),
                "cleanup_session_temp_files must remove cleanup-eligible files: {path}"
            );
        }
        assert!(
            std::path::Path::new(&death_log_path).exists(),
            "cleanup_session_temp_files must NOT remove the death_pane_log snapshot: {death_log_path}"
        );
        assert_eq!(
            std::fs::read_to_string(&death_log_path).unwrap(),
            "post-mortem capture",
            "death_pane_log content must be preserved verbatim"
        );

        let _ = std::fs::remove_file(&death_log_path);
        let _ = std::fs::remove_dir_all(&tdir);
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
    }

    #[test]
    fn tmux_runtime_kind_marker_round_trips_and_cleanup_removes_it() {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");

        let tdir =
            std::env::temp_dir().join(format!("adk-runtime-kind-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
            std::env::set_var("HOSTNAME", "runtime-kind-host");
        }

        let session = format!("runtime-kind-sess-{}", std::process::id());
        write_tmux_runtime_kind_marker(
            &session,
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        )
        .expect("write marker");

        assert_eq!(
            resolve_tmux_runtime_kind_marker(&session),
            Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
        );

        cleanup_session_temp_files(&session);
        assert_eq!(resolve_tmux_runtime_kind_marker(&session), None);

        let _ = std::fs::remove_dir_all(&tdir);
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
    }
}
