//! Newest runtime-activity mtime of a tmux session's relay and provider files.

/// #3053: most-recent runtime activity instant for a tmux session, as a
/// unix-epoch nanosecond count. Considers the relay output (`jsonl`) file mtime,
/// the `.generation` marker mtime, and (best-effort) the provider transcript
/// mtime. These files are touched by the live wrapper/relay even when the
/// session-key heartbeat path silently misses the idle-kill row, so they are a
/// reliable "is this tmux actually doing work?" signal at kill time. Returns 0
/// when nothing is observable.
///
/// #3169: also reused by the stall-watchdog (`recovery.rs`) as a liveness probe
/// before it force-cleans a `desynced` channel — a self-paced loop session that
/// is mid-write touches its jsonl even while the inflight row reads stale, so a
/// fresh probe here means the "desync" is a loop mid-write, not a hang.
pub fn latest_runtime_activity_unix_nanos(tmux_session_name: &str) -> i64 {
    fn mtime_nanos(path: &str) -> i64 {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_nanos()).ok())
            .unwrap_or(0)
    }

    let mut latest = 0i64;

    // Relay output (jsonl) — written by the live wrapper on every provider event.
    if let Some(output_path) =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "jsonl")
    {
        latest = latest.max(mtime_nanos(&output_path));
    }
    // `.generation` marker — written once per spawn; covers fresh sessions whose
    // jsonl has not yet been created.
    if let Some(generation_path) =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "generation")
    {
        latest = latest.max(mtime_nanos(&generation_path));
    }
    // Codex TUI direct mode writes provider-native rollout JSONL under
    // ~/.codex/sessions/... and only stores that path in a small AgentDesk
    // marker. Use the marker and the rollout file as liveness anchors; the
    // AgentDesk relay jsonl may remain quiet while Codex keeps appending output.
    if let Some(marker_path) = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    ) {
        latest = latest.max(mtime_nanos(&marker_path));
    }
    if let Some(marker) =
        crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
    {
        latest = latest.max(marker.rollout_path.to_str().map(mtime_nanos).unwrap_or(0));
    }
    // Claude TUI direct mode relays from the provider-native transcript under
    // ~/.claude/projects. Treat the bound transcript mtime as runtime activity
    // so manual recovery does not keep a stale wrapper jsonl while Claude is
    // actively appending to the real transcript.
    if let Some(binding) =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
        && binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
    {
        latest = latest.max(mtime_nanos(&binding.output_path));
    }

    latest
}
