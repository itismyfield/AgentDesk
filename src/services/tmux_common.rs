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
        // `Tools: 0 done` means a turn has just started (no tools run yet) — a
        // running, not idle, signal — so it must NOT count as idle chrome (it
        // previously let a freshly-submitted running prompt read as ready, #3051).
        // A completed-work footer (`Tools: N>0 done`) or the permission-mode
        // banner are the genuine idle markers; mirrors the `!Tools: 0 done` guard
        // in `..._show_completed_history`.
        line.contains("bypass permissions")
            || (line.contains("Tools:")
                && line.contains(" done")
                && !line.contains("Tools: 0 done"))
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

/// #3107: inflight-INDEPENDENT "the pane is in an active TUI turn" signal.
///
/// A multi-step agentic Claude TUI turn can lose its dcserver inflight mid-turn
/// (a momentary idle observation between tool calls trips the completion gate,
/// commits, and clears inflight) while the pane keeps producing assistant
/// output. Once inflight is gone every later batch is treated as ownerless and
/// suppressed (`should_skip_streaming_placeholder_without_inflight` /
/// `should_suppress_post_terminal_output_without_inflight`), so the live turn
/// goes dark even though the watcher is still alive.
///
/// This predicate gives the suppression/reclaim paths a way to tell a genuinely
/// finished turn (returned to ready-for-input, or showing idle-suggestion
/// chrome — the real post-finish ghost noise we DO want to suppress) apart from
/// a live turn that merely lost its inflight. `true` means: the pane is neither
/// ready-for-input NOR showing idle-suggestion chrome ⇒ it is actively
/// producing ⇒ an active turn (that lost its inflight). `false` means the pane
/// looks finished/idle, so existing suppression must stand.
pub(crate) fn tmux_capture_indicates_claude_tui_actively_streaming(capture: &str) -> bool {
    if capture.trim().is_empty() {
        return false;
    }
    if tmux_capture_indicates_claude_tui_ready_for_input(capture) {
        return false;
    }
    if tmux_capture_indicates_claude_tui_idle_suggestion(capture) {
        return false;
    }
    true
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
            // NOTE: neither the footer context-usage bar (`🤖 Model │ ██░░ │ NN%`)
            // nor the completed-thinking summary line (`✻ Churned for 4m 56s`) is a
            // running signal — both render in IDLE/ready states too. #3051 keyed
            // active-work on the `██` run, which flipped a ready prompt with >20%
            // context usage to not-ready; the running vs. idle distinction is
            // instead carried by the footer (`Tools: 0 done` = freshly-started, no
            // tools yet) handled in `..._show_idle_suggestion_chrome`, plus the
            // explicit `esc to interrupt`/spinner-verb keywords above.
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

/// Detect whether the interactive Claude TUI `/effort` slider overlay is still
/// open in the captured pane.
///
/// Claude Code 2.1.x renders `/effort` as a *horizontal slider*, not a
/// box-drawing radio list: the open overlay carries BOTH an `Effort` heading
/// and a `←/→ to adjust` (left/right arrow) instructional footer. When the
/// overlay is dismissed (Enter confirms the selection) both disappear and the
/// pane returns to the normal composer chrome.
///
/// We require BOTH signals to co-occur in the recent capture so that stale
/// scrollback — e.g. a prior conversation or code snippet that merely mentions
/// `←/→ to adjust` or the word "effort" — cannot be mistaken for a live
/// overlay. Requiring the pair is the load-bearing guard against false
/// "selector still open" failures.
///
/// This is the post-submit validation for `/effort` passthrough: if this
/// returns true after we drive the slider, the selection did NOT confirm and
/// the pane is stranded on the overlay.
pub(crate) fn tmux_capture_indicates_claude_tui_selector_open(capture: &str) -> bool {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_DRAFT_SCAN_LINES);
    let recent = &non_empty[start..];

    let has_footer = recent.iter().any(|line| line_is_slider_adjust_footer(line));
    let has_heading = recent
        .iter()
        .any(|line| line_is_effort_slider_heading(line));
    has_footer && has_heading
}

/// True for the slider's instructional footer, e.g. `←/→ to adjust` or
/// `← / → to adjust` (Claude renders the arrow glyphs `←`/`→` paired with the
/// word "adjust"). We accept either arrow glyph plus the "adjust" keyword so a
/// minor copy/spacing change does not silently disable the detector.
fn line_is_slider_adjust_footer(line: &str) -> bool {
    let lower = trim_prompt_line(line).to_lowercase();
    (lower.contains('←') || lower.contains('→')) && lower.contains("adjust")
}

/// True for the `/effort` slider heading line — the overlay labels the control
/// with the word "effort". Required alongside the adjust footer so a stray
/// scrollback line containing only one of the two signals is not read as a
/// live overlay.
fn line_is_effort_slider_heading(line: &str) -> bool {
    trim_prompt_line(line).to_lowercase().contains("effort")
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
        // #3087: the per-spawn status-panel instance nonce. Must be swept on
        // teardown like the other session temp files — otherwise a respawn whose
        // fresh nonce write fails (logged, non-fatal) would leave the PRIOR
        // spawn's nonce readable, yielding the same instance key as the old
        // spawn and suppressing the panel reset on a genuinely new session.
        // (Mirrors `SPAWN_NONCE_SUFFIX` in discord::tmux_session_files.)
        "spawn_nonce",
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
mod selector_overlay_tests {
    use super::*;

    #[test]
    fn selector_open_detected_for_effort_slider_footer() {
        // Claude Code 2.1.x `/effort` is a horizontal slider with a
        // `←/→ to adjust` footer while the overlay is open.
        let pane = "\
Claude Code v2.1.141

  Effort   low ─ medium ─ [high] ─ xhigh ─ max

  ←/→ to adjust · Enter to confirm · Esc to cancel";

        assert!(tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_detected_with_spaced_arrow_footer() {
        let pane = "\
  Effort
  ← / → to adjust   Enter to confirm";

        assert!(tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_false_when_only_footer_present_in_scrollback() {
        // A stale scrollback line that mentions the adjust footer but has no
        // accompanying Effort heading must not read as a live overlay.
        let pane = "\
Claude Code v2.1.141

  README: press ←/→ to adjust the carousel
❯
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_false_when_only_effort_word_present() {
        // A line that merely mentions "effort" without the adjust footer is
        // not a live slider overlay either.
        let pane = "\
Claude Code v2.1.141

⏺ I adjusted the effort estimate in the doc.
❯
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_false_for_plain_ready_prompt() {
        let pane = "\
Claude Code v2.1.141

❯
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_false_for_composer_draft_mentioning_adjust() {
        // A draft that merely contains the word "adjust" without the slider
        // arrow footer must not be mistaken for an open slider overlay.
        let pane = "\
Claude Code v2.1.141

❯ adjust the layout margins
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done";

        assert!(!tmux_capture_indicates_claude_tui_selector_open(pane));
    }
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

    #[test]
    fn actively_streaming_detects_busy_pane_with_esc_to_interrupt() {
        // #3107: a live agentic turn that lost its inflight — the pane still
        // shows the busy/"esc to interrupt" marker and is producing.
        let capture = "\
⏺ Running 1 shell command…
· Actioning… (4m 7s · esc to interrupt)
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%";

        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
        assert!(tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_rejects_ready_for_input_pane() {
        // A genuinely finished turn returned to ready-for-input: not streaming.
        let capture = "\
✻ Churned for 4m 56s
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%
  CLAUDE.md: 1, MCP: 2 │ Tools: 17 done
  ⏵⏵ bypass permissions on";

        assert!(tmux_capture_indicates_claude_tui_ready_for_input(capture));
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_rejects_idle_suggestion_chrome() {
        // Idle-suggestion chrome is real post-finish ghost noise, not a live
        // turn — must not be treated as actively streaming.
        let capture = "\
⏺ TUI-E2E marker
✻ Worked for 2s
────────────────────────────────────────────────────────────────────────────
❯\u{00a0}좋아, 잘 동작하네
────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ░░░░░░░░░░ │ 4%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(tmux_capture_indicates_claude_tui_idle_suggestion(capture));
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_rejects_empty_capture() {
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(""));
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            "   \n  \n"
        ));
    }
}
