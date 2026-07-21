use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::{CancelToken, ReadOutputResult, cancel_requested};
use parser::{RolloutParseState, process_rollout_line_bytes};
// REQ-006: share the single rollout discovery primitive so `session.rs` and
// `rollout_tail.rs` do not maintain two divergent directory walkers. Tailing
// semantics are unchanged — callers here still apply their own cwd/session/mtime
// filters after discovery.
use super::rollout_index::rollout_files_under;

mod parser;

#[cfg(test)]
use parser::{
    join_streamed_message_boundary, observe_rollout_user_prompt, token_count_status,
    tool_call_message, tool_result_message,
};

const DEFAULT_ROLLOUT_WAIT_SECS: u64 = 30;
/// Fallback EOF drain budget for rollouts that do NOT emit an explicit
/// hook/composer-ready completion signal (legacy Codex CLI versions or
/// unexpected codex variants). Modern Codex TUI completion is driven by
/// `event_msg/composer_ready` (explicit or wrapper-synthetic) or by provider
/// Stop hooks; this drain is only the safety net.
///
/// Issue #2423: previous value of 750ms produced premature `Done` whenever
/// Codex paused for >750ms between rollout writes (e.g. tool-call burst
/// boundary), truncating the assistant response. Issue #2419 / PR #2422 bumped
/// this to 5000ms as a heuristic; #2423 replaced the heuristic with an
/// explicit completion detection and keeps this shorter drain only for
/// legacy/unknown rollout variants. Tool-call gating (see
/// `RolloutParseState::pending_tool_calls`) is the structural complement that
/// suppresses drain entirely while one or more tools are in flight.
const DEFAULT_TERMINAL_DRAIN_MS: u64 = 1000;
/// Issue #2453: legacy Codex CLI builds (no `event_msg` records at all in the
/// rollout) cannot benefit from the explicit completion path nor the
/// token-count refresh signal. The short terminal drain stays structurally
/// fragile against burst-pause-burst patterns with multi-second pauses.
/// When the tail observes ZERO `event_msg` records on a turn — the
/// canonical legacy-CLI fingerprint — the drain budget is bumped to this
/// longer value so a single quiet window must persist that long before the
/// tail flushes `Done`. Modern CLIs always emit at least one `event_msg`
/// (token_count is emitted alongside every message) and therefore retain the
/// shorter base drain. See issue #2453 / PR #2432 follow-up.
const DEFAULT_LEGACY_TERMINAL_DRAIN_MS: u64 = 15000;
/// Upper bound on how long the tailer will sit at EOF waiting for the assistant
/// response to begin streaming. Without this guard, a stuck Codex TUI (tool
/// loop, network hang, etc.) keeps the tailer thread alive indefinitely and the
/// caller never sees a terminal `StreamMessage::Done`.
const DEFAULT_ASSISTANT_RESPONSE_DEADLINE_SECS: u64 = 30 * 60;
/// #2419 follow-up: bounded recovery for a pending tool call whose matching
/// `function_call_output` never arrives (hung tool, malformed line, call_id
/// mismatch, Codex schema skew). Without this, `has_pending_tool_call()`
/// would hold the drain gate shut forever while the tmux pane stays alive,
/// stranding the Discord turn. 5 minutes of inactivity after the last
/// lifecycle event is well past any realistic tool runtime — at that point
/// we surface a terminal Done so the bridge can advance.
const DEFAULT_PENDING_TOOL_CALL_DEADLINE_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutTailOutcome {
    pub lines_read: usize,
    pub bytes_read: u64,
    pub final_offset: u64,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTuiTailResult {
    pub read_result: ReadOutputResult,
    pub rollout_path: PathBuf,
    pub final_offset: u64,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RolloutTailOptions {
    pub wait_for_rollout: Duration,
    pub terminal_drain: Duration,
    /// Maximum time the tailer waits at EOF for the assistant text to begin
    /// streaming. `None` disables the deadline (used by `replay_rollout_file`).
    pub assistant_response_deadline: Option<Duration>,
    /// #2419 follow-up: bounded recovery deadline for a pending tool call
    /// that never resolves (hung tool / malformed line / call_id skew).
    /// Measured as inactivity since the last lifecycle event. `None`
    /// disables the deadline; used by `replay_rollout_file` and by tests
    /// that want the legacy unbounded behaviour.
    pub pending_tool_call_deadline: Option<Duration>,
    /// #2423: when `true` (default), the tail loop interprets an explicit
    /// `event_msg/task_complete` rollout entry — combined with a zero
    /// outstanding tool-call balance — as an immediate `Done` signal,
    /// bypassing `terminal_drain`. Set to `false` to force the legacy
    /// drain-only behaviour as an emergency runtime escape hatch.
    pub enable_task_complete_fast_path: bool,
    /// #2453: extended drain budget applied when the tail observes ZERO
    /// `event_msg` records during the turn (legacy Codex CLI fingerprint).
    /// Modern CLIs emit at least one `event_msg` (typically `token_count`)
    /// per message, so this branch only activates against legacy/unknown
    /// rollout writers. Setting this to `terminal_drain` (or `None`)
    /// restores the pre-#2453 behaviour where a single uniform drain
    /// governs every variant.
    pub legacy_terminal_drain: Option<Duration>,
    /// #2429 (HIGH 2 follow-up): also enforce the pending-tool-call deadline
    /// when no assistant text has been observed yet. Previously the
    /// recovery branch was guarded on `state.saw_assistant_text`, which
    /// meant a tool-first stuck call could pin the turn until the global
    /// `assistant_response_deadline` (30 min). Defaults to `true` so a
    /// tool-only hang surfaces inside the bounded pending-tool budget.
    pub apply_pending_tool_deadline_without_assistant_text: bool,
    /// Optional tmux session owner used to classify rollout user messages as
    /// SSH-direct input versus Discord-routed duplicates.
    pub tmux_session_name: Option<String>,
    /// Discord-origin prompt that launched this Codex TUI turn. Initial
    /// launch/resume can take longer than the short global pending TTL, so
    /// the tail keeps a turn-local copy until the matching rollout user
    /// message is observed.
    pub discord_origin_prompt: Option<String>,
}

impl Default for RolloutTailOptions {
    fn default() -> Self {
        Self {
            wait_for_rollout: Duration::from_secs(DEFAULT_ROLLOUT_WAIT_SECS),
            terminal_drain: Duration::from_millis(DEFAULT_TERMINAL_DRAIN_MS),
            assistant_response_deadline: Some(Duration::from_secs(
                DEFAULT_ASSISTANT_RESPONSE_DEADLINE_SECS,
            )),
            pending_tool_call_deadline: Some(Duration::from_secs(
                DEFAULT_PENDING_TOOL_CALL_DEADLINE_SECS,
            )),
            enable_task_complete_fast_path: true,
            legacy_terminal_drain: Some(Duration::from_millis(DEFAULT_LEGACY_TERMINAL_DRAIN_MS)),
            apply_pending_tool_deadline_without_assistant_text: true,
            tmux_session_name: None,
            discord_origin_prompt: None,
        }
    }
}

pub fn default_codex_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .map(|home| home.join("sessions"))
}

pub(crate) fn observe_rollout_turn_state(
    rollout_path: &Path,
) -> crate::services::tui_turn_state::TuiTurnState {
    crate::services::tui_turn_state::observe_codex_jsonl_turn_state(rollout_path)
}

pub(crate) fn rollout_file_matches_cwd(rollout_path: &Path, cwd: &Path) -> bool {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    rollout_session_cwd_matches(rollout_path, &canonical_cwd)
}

pub fn tail_latest_rollout_for_cwd_with_handoff_for_tmux(
    cwd: &Path,
    modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
    tmux_session_name: &str,
    discord_origin_prompt: Option<&str>,
) -> Result<CodexTuiTailResult, String> {
    let mut options = RolloutTailOptions::default();
    options.tmux_session_name = Some(tmux_session_name.to_string());
    options.discord_origin_prompt = discord_origin_prompt.map(ToString::to_string);
    tail_latest_rollout_for_cwd_with_handoff_options(
        cwd,
        modified_since,
        sender,
        cancel_token,
        is_alive,
        options,
    )
}

fn tail_latest_rollout_for_cwd_with_handoff_options(
    cwd: &Path,
    modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    mut is_alive: impl FnMut() -> bool,
    options: RolloutTailOptions,
) -> Result<CodexTuiTailResult, String> {
    let sessions_dir = default_codex_sessions_dir()
        .ok_or_else(|| "Codex sessions directory is unavailable".to_string())?;
    let rollout_path = wait_for_latest_rollout_for_cwd(
        cwd,
        modified_since,
        &sessions_dir,
        cancel_token.as_deref(),
        &mut is_alive,
        options.wait_for_rollout,
    )?;
    let rollout_session_id =
        super::rollout_index::read_rollout_session_meta(&rollout_path).and_then(|meta| meta.id);
    persist_codex_tui_rollout_marker(
        options.tmux_session_name.as_deref(),
        &rollout_path,
        rollout_session_id.as_deref(),
        Some(0),
    );
    tail_rollout_file_until_assistant_response(
        &rollout_path,
        0,
        None,
        &sender,
        cancel_token,
        is_alive,
        options.terminal_drain,
        options.assistant_response_deadline,
        options.pending_tool_call_deadline,
        options.enable_task_complete_fast_path,
        options.legacy_terminal_drain,
        options.apply_pending_tool_deadline_without_assistant_text,
        options.tmux_session_name,
        options.discord_origin_prompt,
    )
    .map(|(read_result, outcome)| CodexTuiTailResult {
        read_result,
        rollout_path,
        final_offset: outcome.final_offset,
        session_id: outcome.session_id,
    })
}

// #3034: test-only — pins the from-offset replay contract; production replays
// via `tail_rollout_file_from_offset` (which threads cancel/is-alive).
#[allow(dead_code)]
pub fn replay_rollout_file(
    rollout_path: &Path,
    start_offset: u64,
    sender: &Sender<StreamMessage>,
) -> Result<RolloutTailOutcome, String> {
    let (result, outcome) = tail_rollout_file_until_assistant_response(
        rollout_path,
        start_offset,
        None,
        sender,
        None,
        || false,
        Duration::ZERO,
        None,
        None,
        true,
        None,
        true,
        None,
        None,
    )?;
    match result {
        ReadOutputResult::Completed { .. } | ReadOutputResult::SessionDied { .. } => Ok(outcome),
        ReadOutputResult::Cancelled { .. } => Err("rollout replay cancelled".to_string()),
    }
}

pub fn tail_rollout_file_from_offset(
    rollout_path: &Path,
    start_offset: u64,
    session_id: Option<&str>,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
) -> Result<ReadOutputResult, String> {
    let defaults = RolloutTailOptions::default();
    tail_rollout_file_until_assistant_response(
        rollout_path,
        start_offset,
        session_id.map(ToString::to_string),
        &sender,
        cancel_token,
        is_alive,
        defaults.terminal_drain,
        defaults.assistant_response_deadline,
        defaults.pending_tool_call_deadline,
        defaults.enable_task_complete_fast_path,
        defaults.legacy_terminal_drain,
        defaults.apply_pending_tool_deadline_without_assistant_text,
        defaults.tmux_session_name,
        defaults.discord_origin_prompt,
    )
    .map(|result| result.0)
}

/// Tail exactly the rollout already bound to a live Codex TUI pane.
///
/// Unlike the cold resume path, warm follow-up never discovers or switches to
/// another rollout. Eligibility has already pinned `(path, session_id)`, and
/// the caller captures `start_offset` before prompt submission. Keeping the
/// Discord-origin prompt in the turn-local parser state preserves dedupe even
/// if the global pending ledger expires during a long response.
#[allow(clippy::too_many_arguments)]
pub fn tail_warm_followup_rollout_for_tmux(
    rollout_path: &Path,
    start_offset: u64,
    session_id: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
    tmux_session_name: &str,
    discord_origin_prompt: &str,
) -> Result<CodexTuiTailResult, String> {
    persist_codex_tui_rollout_marker(
        Some(tmux_session_name),
        rollout_path,
        Some(session_id),
        Some(start_offset),
    );
    let options = RolloutTailOptions {
        tmux_session_name: Some(tmux_session_name.to_string()),
        discord_origin_prompt: Some(discord_origin_prompt.to_string()),
        ..RolloutTailOptions::default()
    };
    tail_rollout_file_until_assistant_response(
        rollout_path,
        start_offset,
        Some(session_id.to_string()),
        &sender,
        cancel_token,
        is_alive,
        options.terminal_drain,
        options.assistant_response_deadline,
        options.pending_tool_call_deadline,
        options.enable_task_complete_fast_path,
        options.legacy_terminal_drain,
        options.apply_pending_tool_deadline_without_assistant_text,
        options.tmux_session_name,
        options.discord_origin_prompt,
    )
    .map(|(read_result, outcome)| CodexTuiTailResult {
        read_result,
        rollout_path: rollout_path.to_path_buf(),
        final_offset: outcome.final_offset,
        session_id: outcome.session_id,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn tail_resumed_rollout_for_session_with_handoff_for_tmux(
    cwd: &Path,
    session_id: &str,
    previous_rollout_path: &Path,
    previous_start_offset: u64,
    modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
    tmux_session_name: &str,
    discord_origin_prompt: Option<&str>,
) -> Result<CodexTuiTailResult, String> {
    let sessions_dir = default_codex_sessions_dir()
        .ok_or_else(|| "Codex sessions directory is unavailable".to_string())?;
    let mut options = RolloutTailOptions::default();
    options.tmux_session_name = Some(tmux_session_name.to_string());
    options.discord_origin_prompt = discord_origin_prompt.map(ToString::to_string);
    tail_resumed_rollout_for_session_with_handoff_options(
        cwd,
        session_id,
        previous_rollout_path,
        previous_start_offset,
        modified_since,
        &sessions_dir,
        sender,
        cancel_token,
        is_alive,
        options,
    )
}

// #3034: test-only — the non-handoff resumed-tail variant is exercised by the
// resume regression tests; production uses the `_with_handoff_for_tmux` form.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
fn tail_resumed_rollout_for_session_with_options(
    cwd: &Path,
    session_id: &str,
    previous_rollout_path: &Path,
    previous_start_offset: u64,
    modified_since: SystemTime,
    sessions_dir: &Path,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
    options: RolloutTailOptions,
) -> Result<ReadOutputResult, String> {
    tail_resumed_rollout_for_session_with_handoff_options(
        cwd,
        session_id,
        previous_rollout_path,
        previous_start_offset,
        modified_since,
        sessions_dir,
        sender,
        cancel_token,
        is_alive,
        options,
    )
    .map(|result| result.read_result)
}

#[allow(clippy::too_many_arguments)]
fn tail_resumed_rollout_for_session_with_handoff_options(
    cwd: &Path,
    session_id: &str,
    previous_rollout_path: &Path,
    previous_start_offset: u64,
    modified_since: SystemTime,
    sessions_dir: &Path,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    mut is_alive: impl FnMut() -> bool,
    options: RolloutTailOptions,
) -> Result<CodexTuiTailResult, String> {
    let rollout_path = wait_for_resumed_rollout_for_session(
        cwd,
        session_id,
        previous_rollout_path,
        previous_start_offset,
        modified_since,
        sessions_dir,
        cancel_token.as_deref(),
        &mut is_alive,
        options.wait_for_rollout,
    )?;
    let start_offset = if same_path(&rollout_path, previous_rollout_path) {
        previous_start_offset
    } else {
        0
    };
    persist_codex_tui_rollout_marker(
        options.tmux_session_name.as_deref(),
        &rollout_path,
        Some(session_id),
        Some(start_offset),
    );
    let known_session_id = (start_offset > 0).then(|| session_id.to_string());
    tail_rollout_file_until_assistant_response(
        &rollout_path,
        start_offset,
        known_session_id,
        &sender,
        cancel_token,
        is_alive,
        options.terminal_drain,
        options.assistant_response_deadline,
        options.pending_tool_call_deadline,
        options.enable_task_complete_fast_path,
        options.legacy_terminal_drain,
        options.apply_pending_tool_deadline_without_assistant_text,
        options.tmux_session_name,
        options.discord_origin_prompt,
    )
    .map(|(read_result, outcome)| CodexTuiTailResult {
        read_result,
        rollout_path,
        final_offset: outcome.final_offset,
        session_id: outcome.session_id,
    })
}

fn persist_codex_tui_rollout_marker(
    tmux_session_name: Option<&str>,
    rollout_path: &Path,
    session_id: Option<&str>,
    rollout_start_offset: Option<u64>,
) {
    let Some(tmux_session_name) = tmux_session_name else {
        return;
    };
    if let Err(error) =
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            tmux_session_name,
            rollout_path,
            session_id,
            rollout_start_offset,
        )
    {
        tracing::warn!(
            tmux_session_name,
            rollout_path = %rollout_path.display(),
            error,
            "failed to persist Codex TUI rollout marker after transcript discovery"
        );
    }
}

fn wait_for_latest_rollout_for_cwd(
    cwd: &Path,
    modified_since: SystemTime,
    sessions_dir: &Path,
    cancel_token: Option<&CancelToken>,
    is_alive: &mut impl FnMut() -> bool,
    timeout: Duration,
) -> Result<PathBuf, String> {
    let started = Instant::now();
    loop {
        if cancel_requested(cancel_token) {
            return Err("cancelled waiting for Codex rollout transcript".to_string());
        }
        if let Some(path) = latest_rollout_for_cwd_since(cwd, modified_since, sessions_dir) {
            return Ok(path);
        }
        if !is_alive() {
            return Err("Codex TUI exited before creating a rollout transcript".to_string());
        }
        if started.elapsed() > timeout {
            return Err(format!(
                "Timeout waiting for Codex rollout transcript under {}",
                sessions_dir.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[allow(clippy::too_many_arguments)]
fn wait_for_resumed_rollout_for_session(
    cwd: &Path,
    session_id: &str,
    previous_rollout_path: &Path,
    previous_start_offset: u64,
    modified_since: SystemTime,
    sessions_dir: &Path,
    cancel_token: Option<&CancelToken>,
    is_alive: &mut impl FnMut() -> bool,
    timeout: Duration,
) -> Result<PathBuf, String> {
    let started = Instant::now();
    loop {
        if cancel_requested(cancel_token) {
            return Err("cancelled waiting for Codex resumed rollout transcript".to_string());
        }
        if rollout_file_len(previous_rollout_path).is_some_and(|len| len > previous_start_offset) {
            return Ok(previous_rollout_path.to_path_buf());
        }
        if let Some(path) =
            latest_rollout_for_cwd_and_session_since(cwd, session_id, modified_since, sessions_dir)
        {
            return Ok(path);
        }
        if !is_alive() {
            return Err(
                "Codex TUI exited before updating a resumed rollout transcript".to_string(),
            );
        }
        if started.elapsed() > timeout {
            return Err(format!(
                "Timeout waiting for Codex resumed rollout transcript under {}",
                sessions_dir.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Newest rollout transcript under `sessions_dir` whose `session_meta` cwd
/// matches `cwd` and whose mtime is at/after `modified_since`.
///
/// Routed through [`cached_indexed_rollouts`](super::rollout_index::cached_indexed_rollouts)
/// so the follow-up readiness path (`tui_followup.rs`, which has no provider
/// session id) and the launch wait loop reuse the process-lifetime index instead
/// of re-walking `~/.codex/sessions` and re-parsing every header on every probe.
/// Selection semantics are identical to the legacy direct scan: the index
/// supplies the same `(mtime, len)` projection and parsed `session_meta`, and the
/// cwd match still canonicalizes the header's raw cwd against `cwd` exactly as
/// the former `rollout_session_cwd_matches` did. Files with no parseable
/// `session_meta` carry `meta == None` in the index and are skipped, matching the
/// old `false` return from the header scan.
pub fn latest_rollout_for_cwd_since(
    cwd: &Path,
    modified_since: SystemTime,
    sessions_dir: &Path,
) -> Option<PathBuf> {
    rollout_candidates_for_cwd_since(cwd, modified_since, sessions_dir)
        .into_iter()
        .next()
}

pub fn latest_unclaimed_rollout_for_cwd_since(
    cwd: &Path,
    modified_since: SystemTime,
    sessions_dir: &Path,
    claimed_rollout_paths: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    rollout_candidates_for_cwd_since(cwd, modified_since, sessions_dir)
        .into_iter()
        .find(|path| !rollout_path_is_claimed(path, claimed_rollout_paths))
}

pub fn rollout_candidates_for_cwd_since(
    cwd: &Path,
    modified_since: SystemTime,
    sessions_dir: &Path,
) -> Vec<PathBuf> {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    for item in super::rollout_index::cached_indexed_rollouts(sessions_dir) {
        if item.modified < modified_since {
            continue;
        }
        let Some(meta) = item.meta.as_ref() else {
            continue;
        };
        let session_cwd =
            std::fs::canonicalize(&meta.cwd).unwrap_or_else(|_| PathBuf::from(&meta.cwd));
        if session_cwd != canonical_cwd {
            continue;
        }
        candidates.push((item.modified, item.path));
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    candidates.into_iter().map(|(_, path)| path).collect()
}

fn rollout_path_is_claimed(path: &Path, claimed_rollout_paths: &HashSet<PathBuf>) -> bool {
    if claimed_rollout_paths.contains(path) {
        return true;
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    claimed_rollout_paths.contains(&canonical)
}

/// Find a codex rollout transcript by its session UUID alone, scanning the
/// configured codex sessions directory. Used by recovery / restore paths after
/// a dcserver restart: the inflight row carries `session_id` but its stored
/// `output_path` may point at the AgentDesk-side relay JSONL that does not
/// exist for codex_tui (codex writes directly to `~/.codex/sessions/...`).
/// Returns the first matching `rollout-*-<session_id>.jsonl` whose filename
/// ends with the session UUID — codex assigns one rollout per session id.
pub fn find_rollout_by_session_id(session_id: &str) -> Option<PathBuf> {
    let sessions_dir = default_codex_sessions_dir()?;
    find_rollout_by_session_id_under(&sessions_dir, session_id)
}

pub(crate) fn find_rollout_by_session_id_under(
    sessions_dir: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let suffix = format!("-{trimmed}.jsonl");
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for path in rollout_files_under(sessions_dir) {
        let matches = path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.ends_with(&suffix));
        if !matches {
            continue;
        }
        let modified = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if best
            .as_ref()
            .is_none_or(|(best_modified, _)| modified >= *best_modified)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

fn latest_rollout_for_cwd_and_session_since(
    cwd: &Path,
    session_id: &str,
    modified_since: SystemTime,
    sessions_dir: &Path,
) -> Option<PathBuf> {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for path in rollout_files_under(sessions_dir) {
        let Some(modified) = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
        else {
            continue;
        };
        if modified < modified_since {
            continue;
        }
        if !rollout_session_meta_matches(&path, &canonical_cwd, Some(session_id)) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(best_modified, _)| modified > *best_modified)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

fn rollout_session_cwd_matches(path: &Path, cwd: &Path) -> bool {
    rollout_session_meta_matches(path, cwd, None)
}

fn rollout_session_meta_matches(path: &Path, cwd: &Path, session_id: Option<&str>) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok).take(20) {
        let Ok(json) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if json.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = json.get("payload") else {
            continue;
        };
        if let Some(expected_session_id) = session_id {
            let Some(actual_session_id) = payload.get("id").and_then(Value::as_str) else {
                continue;
            };
            if actual_session_id != expected_session_id {
                continue;
            }
        }
        let Some(raw_cwd) = payload.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        let session_cwd = std::fs::canonicalize(raw_cwd).unwrap_or_else(|_| PathBuf::from(raw_cwd));
        return session_cwd == cwd;
    }
    false
}

fn rollout_file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|metadata| metadata.len())
}

fn same_path(left: &Path, right: &Path) -> bool {
    let canonical_left = std::fs::canonicalize(left).ok();
    let canonical_right = std::fs::canonicalize(right).ok();
    match (canonical_left, canonical_right) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

#[allow(clippy::too_many_arguments)]
fn tail_rollout_file_until_assistant_response(
    rollout_path: &Path,
    start_offset: u64,
    initial_session_id: Option<String>,
    sender: &Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    mut is_alive: impl FnMut() -> bool,
    terminal_drain: Duration,
    assistant_response_deadline: Option<Duration>,
    pending_tool_call_deadline: Option<Duration>,
    enable_task_complete_fast_path: bool,
    legacy_terminal_drain: Option<Duration>,
    apply_pending_tool_deadline_without_assistant_text: bool,
    tmux_session_name: Option<String>,
    discord_origin_prompt: Option<String>,
) -> Result<(ReadOutputResult, RolloutTailOutcome), String> {
    let mut file = std::fs::File::open(rollout_path)
        .map_err(|error| format!("open Codex rollout {}: {error}", rollout_path.display()))?;
    let file_len = file
        .metadata()
        .map_err(|error| format!("stat Codex rollout {}: {error}", rollout_path.display()))?
        .len();
    let seek_offset = start_offset.min(file_len);
    file.seek(SeekFrom::Start(seek_offset))
        .map_err(|error| format!("seek Codex rollout {}: {error}", rollout_path.display()))?;

    let mut state = RolloutParseState {
        session_id: initial_session_id,
        tmux_session_name,
        discord_origin_prompt,
        ..RolloutParseState::default()
    };
    let mut current_offset = seek_offset;
    let mut partial_line = Vec::new();
    let mut buf = [0u8; 8192];
    let mut last_output_at: Option<Instant> = None;
    let started_at = Instant::now();
    let mut hook_events = crate::services::claude_tui::hook_server::subscribe_hook_events();
    // #2172 cancel boundary: wrap the raw sender so any send after the
    // shared `cancel_token` flips becomes a no-op. The producer (this
    // tail) is the relay-suppression enforcement point — once the user
    // cancels a turn, no further rollout-derived StreamMessage may reach
    // the bridge / Discord for that turn. See
    // docs/codex-tui-cancel-boundary.md for the full contract.
    let sender = RelaySuppressionSender::new(sender, cancel_token.as_deref());

    loop {
        if sender.cancel_observed() {
            return Ok((
                ReadOutputResult::Cancelled {
                    offset: current_offset,
                },
                outcome(&state, current_offset),
            ));
        }

        match file.read(&mut buf) {
            Ok(0) => {
                if try_process_complete_partial_line(&mut partial_line, &sender, &mut state) {
                    last_output_at = Some(Instant::now());
                    continue;
                }
                observe_codex_completion_hooks(&mut hook_events, &mut state);
                if let Some(finalize_path) =
                    explicit_finalize_path(&mut state, enable_task_complete_fast_path)
                {
                    emit_done(&sender, &state, finalize_path, rollout_path, current_offset);
                    return Ok((
                        ReadOutputResult::Completed {
                            offset: current_offset,
                        },
                        outcome(&state, current_offset),
                    ));
                }
                // #2419: only consider the turn drainable when no tool call
                // is currently in flight. Otherwise the natural silence while
                // codex waits for the tool result would trip the timer.
                //
                // #2453: with NO `event_msg` observed yet (legacy CLI
                // fingerprint), bump to `legacy_terminal_drain`. Modern CLIs
                // emit ≥1 `event_msg` per message, so this widens the drain
                // only for legacy writers lacking the structural signals
                // (token_count refresh, hook/envelope completion) the short
                // default leans on. Applied only when legacy drain is strictly
                // longer — `terminal_drain.is_zero()` (replay) / shorter
                // overrides stay verbatim.
                let effective_drain = if state.seen_any_event_msg || terminal_drain.is_zero() {
                    terminal_drain
                } else {
                    match legacy_terminal_drain {
                        Some(legacy) if legacy > terminal_drain => legacy,
                        _ => terminal_drain,
                    }
                };
                if state.saw_assistant_text && !state.has_pending_tool_call() {
                    if effective_drain.is_zero()
                        || last_output_at.is_some_and(|at| at.elapsed() >= effective_drain)
                    {
                        if heuristic_finalize_allowed(&mut state, rollout_path, current_offset) {
                            emit_done(
                                &sender,
                                &state,
                                RolloutFinalizePath::Heuristic,
                                rollout_path,
                                current_offset,
                            );
                            return Ok((
                                ReadOutputResult::Completed {
                                    offset: current_offset,
                                },
                                outcome(&state, current_offset),
                            ));
                        }
                        last_output_at = Some(Instant::now());
                    }
                }
                // #2419 (Codex HIGH): bounded recovery for a pending tool call
                // whose `*_output` never arrives (hung tool / malformed line /
                // call_id skew) — else the drain gate stays shut forever while
                // the pane lives and the Discord turn hangs. After
                // `pending_tool_call_deadline` of inactivity past the last
                // lifecycle event we surface a terminal Done. #2429 HIGH 2:
                // with `apply_pending_tool_deadline_without_assistant_text`
                // (default) it ALSO fires for a tool that hangs BEFORE any
                // assistant text (previously fell to the 30 min global); the
                // warning copy flags the no-text case for operators.
                let tool_deadline_gate = state.has_pending_tool_call()
                    && (state.saw_assistant_text
                        || apply_pending_tool_deadline_without_assistant_text);
                if tool_deadline_gate
                    && let Some(deadline) = pending_tool_call_deadline
                    && last_output_at.is_some_and(|at| at.elapsed() >= deadline)
                {
                    let elapsed_secs = last_output_at
                        .map(|at| at.elapsed().as_secs())
                        .unwrap_or_default();
                    let tool_first = !state.saw_assistant_text;
                    tracing::warn!(
                        rollout_path = %rollout_path.display(),
                        elapsed_secs,
                        pending_keyed = state.pending_tool_calls.len(),
                        pending_unkeyed = state.pending_tool_calls_unkeyed,
                        tool_first,
                        "Codex rollout tail tool-call deadline expired; emitting Done to unblock turn"
                    );
                    let mut result_text = state.final_text.clone();
                    let warning = if tool_first {
                        format!(
                            "⚠ Codex tool call did not resolve within {}s before any assistant text — emitting empty response to unblock turn.",
                            elapsed_secs
                        )
                    } else {
                        format!(
                            "\n\n⚠ Codex tool call did not resolve within {}s — emitting partial response.",
                            elapsed_secs
                        )
                    };
                    if tool_first {
                        result_text = warning;
                    } else {
                        result_text.push_str(&warning);
                    }
                    sender.send(StreamMessage::Done {
                        result: result_text,
                        session_id: state.session_id.clone(),
                    });
                    return Ok((
                        ReadOutputResult::Completed {
                            offset: current_offset,
                        },
                        outcome(&state, current_offset),
                    ));
                }
                if !is_alive() {
                    let result = if state.saw_assistant_text {
                        emit_done(
                            &sender,
                            &state,
                            RolloutFinalizePath::Heuristic,
                            rollout_path,
                            current_offset,
                        );
                        ReadOutputResult::Completed {
                            offset: current_offset,
                        }
                    } else {
                        ReadOutputResult::SessionDied {
                            offset: current_offset,
                        }
                    };
                    return Ok((result, outcome(&state, current_offset)));
                }
                // #2182: hung-TUI guard (else the tailer lives forever, no
                // `Done`). #3419 B: fire on IDLE, not absolute age — silence
                // since the last rollout activity (`last_output_at`: assistant
                // text AND tool/event lifecycle, NOT empty 100ms polls), or
                // `started_at` while no output ever arrived. A live codex turn
                // (write_stdin/tool/event) resets idle and survives; only a
                // genuinely silent turn trips `deadline`. `!saw_assistant_text`
                // keeps this the no-text branch (post-text drain owns the rest).
                let idle_elapsed =
                    last_output_at.map_or_else(|| started_at.elapsed(), |at| at.elapsed());
                if !state.saw_assistant_text
                    && let Some(deadline) = assistant_response_deadline
                    && idle_elapsed >= deadline
                {
                    let elapsed_secs = idle_elapsed.as_secs();
                    tracing::warn!(
                        rollout_path = %rollout_path.display(),
                        elapsed_secs,
                        "Codex rollout tail idle past deadline with no assistant response; emitting Done"
                    );
                    sender.send(StreamMessage::Done {
                        result: format!(
                            "⚠ Codex TUI produced no assistant response within {}s — turn timed out.",
                            elapsed_secs
                        ),
                        session_id: state.session_id.clone(),
                    });
                    return Ok((
                        ReadOutputResult::Completed {
                            offset: current_offset,
                        },
                        outcome(&state, current_offset),
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(n) => {
                current_offset += n as u64;
                partial_line.extend_from_slice(&buf[..n]);
                // #2172 cancel boundary: a single `read` may return many
                // newline-delimited rollout records. Without this check,
                // every line drained from the just-read buffer would be
                // pushed to the sender BEFORE the next loop iteration
                // observes cancel — even if the user has already pressed
                // /stop. The `RelaySuppressionSender` is the canonical
                // enforcement point: once the shared cancel flag flips,
                // every `send` call drops on the floor.
                while let Some(pos) = partial_line.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = partial_line.drain(..=pos).collect();
                    state.record(line.len());
                    if process_rollout_line_bytes(&line, &sender, &mut state) {
                        last_output_at = Some(Instant::now());
                    }
                }
            }
            Err(error) => {
                return Err(format!(
                    "read Codex rollout {}: {error}",
                    rollout_path.display()
                ));
            }
        }
    }
}

/// Relay suppression wrapper for `Sender<StreamMessage>` used by the Codex
/// TUI rollout tail.
///
/// Contract (see docs/codex-tui-cancel-boundary.md):
/// - Once the shared `CancelToken` is flipped to cancelled, every subsequent
///   `send` is dropped silently. There is no "drain the in-flight assistant
///   text first" carve-out: a cancel is a hard relay boundary.
/// - This is the single producer-side enforcement point. The bridge / watcher
///   consumers will also drain `rx` after cancel but the canonical guarantee
///   that no post-cancel `StreamMessage` is emitted lives here, in the only
///   thread that owns the rollout file.
/// - `cancel_observed()` is a snapshot of the shared flag and is used by the
///   read loop to decide whether to return `ReadOutputResult::Cancelled` —
///   it MUST remain consistent with the actual `send` suppression so a tail
///   that returned `Cancelled` is guaranteed to have stopped emitting.
struct RelaySuppressionSender<'a> {
    inner: &'a Sender<StreamMessage>,
    cancel_token: Option<&'a CancelToken>,
}

impl<'a> RelaySuppressionSender<'a> {
    fn new(inner: &'a Sender<StreamMessage>, cancel_token: Option<&'a CancelToken>) -> Self {
        Self {
            inner,
            cancel_token,
        }
    }

    fn cancel_observed(&self) -> bool {
        cancel_requested(self.cancel_token)
    }

    fn send(&self, message: StreamMessage) {
        if self.cancel_observed() {
            // Post-cancel relay suppression. Dropping the message here is
            // intentional: the cancelled turn must not emit any further
            // StreamMessage so the bridge does not relay it to Discord or
            // mutate inflight state on its behalf.
            return;
        }
        let _ = self.inner.send(message);
    }
}

fn try_process_complete_partial_line(
    partial_line: &mut Vec<u8>,
    sender: &RelaySuppressionSender<'_>,
    state: &mut RolloutParseState,
) -> bool {
    let Ok(line) = std::str::from_utf8(partial_line) else {
        return false;
    };
    let trimmed = line.trim();
    if trimmed.is_empty() || serde_json::from_str::<Value>(trimmed).is_err() {
        return false;
    }
    let line = std::mem::take(partial_line);
    state.record(line.len());
    process_rollout_line_bytes(&line, sender, state)
}

fn outcome(state: &RolloutParseState, final_offset: u64) -> RolloutTailOutcome {
    RolloutTailOutcome {
        lines_read: state.lines_read,
        bytes_read: state.bytes_read,
        final_offset,
        session_id: state.session_id.clone(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RolloutFinalizePath {
    Hook,
    Envelope,
    Heuristic,
}

impl RolloutFinalizePath {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::Envelope => "envelope",
            Self::Heuristic => "heuristic",
        }
    }
}

fn observe_codex_completion_hooks(
    hook_events: &mut tokio::sync::broadcast::Receiver<
        crate::services::claude_tui::hook_server::HookEvent,
    >,
    state: &mut RolloutParseState,
) {
    loop {
        match hook_events.try_recv() {
            Ok(event) => {
                if codex_completion_hook_matches(&event, state) {
                    state.hook_completion_seen = true;
                    state.lifecycle_activity = true;
                    tracing::debug!(
                        session_id = %event.session_id,
                        event = event.kind.as_str(),
                        "codex rollout tail observed completion hook"
                    );
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "codex rollout tail lagged while reading hook completion events"
                );
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
}

fn codex_completion_hook_matches(
    event: &crate::services::claude_tui::hook_server::HookEvent,
    state: &RolloutParseState,
) -> bool {
    if event.provider != "codex" {
        return false;
    }
    if !matches!(
        event.kind,
        crate::services::claude_tui::hook_server::HookEventKind::Stop
            | crate::services::claude_tui::hook_server::HookEventKind::SubagentStop
    ) {
        return false;
    }
    state
        .session_id
        .as_deref()
        .is_some_and(|session_id| session_id == event.session_id)
        || state
            .tmux_session_name
            .as_deref()
            .is_some_and(|tmux_session_name| tmux_session_name == event.session_id)
}

fn explicit_finalize_path(
    state: &mut RolloutParseState,
    enable_task_complete_fast_path: bool,
) -> Option<RolloutFinalizePath> {
    if state.has_pending_tool_call() {
        return None;
    }

    if state.hook_completion_seen && state.saw_assistant_text {
        return Some(RolloutFinalizePath::Hook);
    }

    let envelope_completion_seen = state.explicit_composer_ready_seen
        || (enable_task_complete_fast_path && state.synthetic_composer_ready_seen);
    if !envelope_completion_seen {
        return None;
    }

    promote_task_complete_fallback_text(state);

    // Schema-drift guard: an explicit terminal signal without any assistant
    // text is not enough to emit an empty Done. Fall through to the heuristic
    // fallback so any subsequent assistant text still has a chance to arrive.
    if !state.saw_assistant_text {
        if !state.explicit_completion_missing_text_warned {
            tracing::warn!(
                hook_completion_seen = state.hook_completion_seen,
                explicit_composer_ready_seen = state.explicit_composer_ready_seen,
                synthetic_composer_ready_seen = state.synthetic_composer_ready_seen,
                "codex rollout explicit completion signal missing assistant text; falling back to drain"
            );
            state.explicit_completion_missing_text_warned = true;
        }
        return None;
    }

    Some(RolloutFinalizePath::Envelope)
}

fn promote_task_complete_fallback_text(state: &mut RolloutParseState) {
    let Some(text) = state.task_complete_fallback_text.as_deref() else {
        return;
    };

    // Recover the assistant text from `last_agent_message` when the turn
    // produced no `response_item/message` (tool-only turns or rollouts where
    // the assistant text is only carried on `task_complete`).
    //
    // #3343 r2 review P2: commentary-only turns now MIRROR commentary into
    // `final_text` without setting `saw_assistant_text`, and `last_agent_message`
    // typically carries that same commentary body — a blind append here would
    // duplicate it. The fallback is ALWAYS consumed and `saw_assistant_text`
    // set (the turn has an assistant-visible body; finalize must not time out),
    // but the text lands at most once: empty `final_text` appends, a superset
    // replaces the mirrored commentary, an already-mirrored body (equal or a
    // message-boundary suffix) is dropped, and anything else appends
    // boundary-joined. #3343 r3: arbitrary substring containment is NOT a
    // drop — a short canonical terminal body embedded mid-sentence in
    // commentary is a genuinely new body.
    if !state.saw_assistant_text {
        let text = state
            .task_complete_fallback_text
            .take()
            .expect("task_complete fallback checked above");
        if state.final_text.is_empty() {
            // #3343: route the fallback body through the same shared boundary
            // writer so `final_text` follows the suppress-on-existing-newline
            // rule here too.
            state.push_message_text(&text);
        } else if task_complete_fallback_supersedes_final_text(&state.final_text, &text) {
            state.final_text = text;
        } else if !task_complete_fallback_already_mirrored(&state.final_text, &text) {
            state.push_message_text(&text);
        }
        state.saw_assistant_text = true;
        return;
    }

    // Codex TUI rollout can stream only the visible tail through
    // response_item/message while task_complete.last_agent_message carries the
    // full provider terminal body. Promote that authoritative body before
    // Done.result is emitted so turn_bridge and session-bound relay receive the
    // same complete BEGIN/MID/END response.
    if task_complete_fallback_supersedes_final_text(&state.final_text, text) {
        let previous_final_text_len = state.final_text.len();
        let text = state
            .task_complete_fallback_text
            .take()
            .expect("task_complete fallback checked above");
        tracing::info!(
            target: "agentdesk::codex_rollout_handoff",
            previous_final_text_len,
            task_complete_fallback_len = text.len(),
            "codex rollout promoted task_complete last_agent_message over streamed tail"
        );
        state.final_text = text;
    }
}

fn task_complete_fallback_supersedes_final_text(final_text: &str, fallback_text: &str) -> bool {
    let streamed = final_text.trim();
    let fallback = fallback_text.trim();
    !streamed.is_empty() && fallback.len() > streamed.len() && fallback.ends_with(streamed)
}

// The fallback counts as already mirrored only when it IS the final text or
// sits at the end after a message boundary — a mid-sentence substring match
// (e.g. commentary quoting the terminal verdict) must still append. The
// boundary tolerates horizontal whitespace after the newline (r4 P3: an
// indented mirrored body must still drop).
fn task_complete_fallback_already_mirrored(final_text: &str, fallback_text: &str) -> bool {
    let streamed = final_text.trim();
    let fallback = fallback_text.trim();
    if fallback.is_empty() {
        return true;
    }
    let Some(prefix) = streamed.strip_suffix(fallback) else {
        return false;
    };
    let boundary = prefix.trim_end_matches([' ', '\t']);
    boundary.is_empty() || boundary.ends_with('\n')
}

fn heuristic_finalize_allowed(
    state: &mut RolloutParseState,
    rollout_path: &Path,
    offset: u64,
) -> bool {
    let Some(tmux_session_name) = state.tmux_session_name.as_deref() else {
        return true;
    };
    if !state.seen_any_event_msg {
        return true;
    }

    if !state.heuristic_finalize_waiting_for_completion_logged {
        tracing::warn!(
            rollout_path = %rollout_path.display(),
            offset,
            tmux_session_name,
            "codex rollout heuristic Done deferred until explicit modern TUI completion"
        );
        state.heuristic_finalize_waiting_for_completion_logged = true;
    }
    false
}

fn emit_done(
    sender: &RelaySuppressionSender<'_>,
    state: &RolloutParseState,
    finalize_path: RolloutFinalizePath,
    rollout_path: &Path,
    offset: u64,
) {
    tracing::info!(
        rollout_path = %rollout_path.display(),
        offset,
        finalize_path = finalize_path.as_str(),
        session_id = state.session_id.as_deref(),
        lines_read = state.lines_read,
        bytes_read = state.bytes_read,
        saw_assistant_text = state.saw_assistant_text,
        hook_completion_seen = state.hook_completion_seen,
        composer_ready_seen = state.composer_ready_seen,
        final_text_len = state.final_text.len(),
        task_complete_fallback_len = state
            .task_complete_fallback_text
            .as_deref()
            .map(str::len)
            .unwrap_or(0),
        "codex rollout tail emitting Done"
    );
    sender.send(StreamMessage::Done {
        result: state.final_text.clone(),
        session_id: state.session_id.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;

    fn collect_rollout(lines: &str, start_offset: u64) -> Vec<StreamMessage> {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), lines).unwrap();
        let (tx, rx) = mpsc::channel();

        replay_rollout_file(file.path(), start_offset, &tx).unwrap();
        drop(tx);
        rx.iter().collect()
    }

    // #2795 — `find_rollout_by_session_id_under` must match codex rollout
    // filenames by their trailing `-<session_id>.jsonl` suffix so dcserver
    // recovery can re-attach to the live codex pane after a mid-turn restart
    // when the AgentDesk-side relay JSONL is missing.
    #[test]
    fn find_rollout_by_session_id_matches_uuid_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("2026").join("05").join("27");
        std::fs::create_dir_all(&sub).unwrap();
        let target =
            sub.join("rollout-2026-05-27T05-50-07-019e660d-4859-7522-9cee-8ba7c4e7c743.jsonl");
        std::fs::write(&target, "{}\n").unwrap();
        let unrelated =
            sub.join("rollout-2026-05-27T05-50-07-deadbeef-0000-0000-0000-000000000000.jsonl");
        std::fs::write(&unrelated, "{}\n").unwrap();

        let resolved =
            find_rollout_by_session_id_under(dir.path(), "019e660d-4859-7522-9cee-8ba7c4e7c743");
        assert_eq!(resolved.as_deref(), Some(target.as_path()));
    }

    #[test]
    fn find_rollout_by_session_id_returns_none_for_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let resolved =
            find_rollout_by_session_id_under(dir.path(), "00000000-0000-0000-0000-000000000000");
        assert!(resolved.is_none());
    }

    #[test]
    fn find_rollout_by_session_id_rejects_empty_input() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("2026").join("05").join("27");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("rollout-2026-05-27T05-50-07-019e660d-4859-7522-9cee-8ba7c4e7c743.jsonl"),
            "{}\n",
        )
        .unwrap();
        assert!(find_rollout_by_session_id_under(dir.path(), "").is_none());
        assert!(find_rollout_by_session_id_under(dir.path(), "   ").is_none());
    }

    // U-8 tool_call_message must drop entries with no usable `name` field
    // — a function_call envelope without a name is not a renderable tool
    // event and must not surface as a placeholder ToolUse.
    #[test]
    fn tool_call_with_missing_name_yields_no_emit() {
        let payload = serde_json::json!({ "arguments": "{}" });
        assert!(tool_call_message(&payload).is_none());

        let payload = serde_json::json!({ "name": "   ", "arguments": "{}" });
        assert!(tool_call_message(&payload).is_none());
    }

    // U-8 tool_call_message accepts the modern `arguments` field and falls
    // back to `input` then `action` for legacy variants, preserving the
    // payload as compact JSON.
    #[test]
    fn tool_call_argument_fallback_order_is_preserved() {
        let modern = serde_json::json!({ "name": "exec", "arguments": "{\"cmd\":\"ls\"}" });
        let legacy_input = serde_json::json!({ "name": "exec", "input": {"cmd": "ls"} });
        let legacy_action = serde_json::json!({ "name": "exec", "action": "ls" });

        for payload in [&modern, &legacy_input, &legacy_action] {
            let msg = tool_call_message(payload).expect("tool_call_message emits");
            match msg {
                StreamMessage::ToolUse { name, input, .. } => {
                    assert_eq!(name, "exec");
                    assert!(!input.is_empty());
                }
                other => panic!("expected ToolUse, got {other:?}"),
            }
        }
    }

    // U-8 tool_result_message drops empty payloads so the relay does not
    // emit blank result lines that the user would see as noise.
    #[test]
    fn tool_result_with_empty_content_yields_no_emit() {
        let payload = serde_json::json!({ "output": "" });
        assert!(tool_result_message(&payload).is_none());

        let payload = serde_json::json!({});
        assert!(tool_result_message(&payload).is_none());
    }

    // U-8 tool_result_message preserves the `is_error` flag from either
    // snake_case or camelCase form, defaulting to false when neither is
    // set.
    #[test]
    fn tool_result_is_error_flag_is_preserved_from_either_naming() {
        for payload in [
            serde_json::json!({ "output": "boom", "is_error": true }),
            serde_json::json!({ "output": "boom", "isError": true }),
        ] {
            match tool_result_message(&payload).expect("tool_result_message emits") {
                StreamMessage::ToolResult {
                    is_error, content, ..
                } => {
                    assert!(is_error);
                    assert_eq!(content, "boom");
                }
                other => panic!("expected ToolResult, got {other:?}"),
            }
        }

        let payload = serde_json::json!({ "output": "ok" });
        match tool_result_message(&payload).expect("tool_result_message emits") {
            StreamMessage::ToolResult { is_error, .. } => assert!(!is_error),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    fn write_rollout(root: &Path, relative: &str, id: &str, cwd: &Path, body: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"cwd\":\"{}\"}}}}\n{}",
                id,
                cwd.display(),
                body
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn latest_unclaimed_rollout_skips_rollout_claimed_by_another_tui() {
        let _guard = super::super::rollout_index::lock_cache_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let older = write_rollout(
            dir.path(),
            "old/rollout-old.jsonl",
            "session-old",
            cwd.path(),
            "",
        );
        let newer = write_rollout(
            dir.path(),
            "new/rollout-new.jsonl",
            "session-new",
            cwd.path(),
            "",
        );
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        filetime::set_file_mtime(
            &older,
            filetime::FileTime::from_system_time(base + Duration::from_secs(10)),
        )
        .unwrap();
        filetime::set_file_mtime(
            &newer,
            filetime::FileTime::from_system_time(base + Duration::from_secs(20)),
        )
        .unwrap();

        assert_eq!(
            latest_rollout_for_cwd_since(cwd.path(), base, dir.path()).as_deref(),
            Some(newer.as_path()),
            "precondition: unclaimed lookup chooses the newest rollout"
        );

        let claimed = [std::fs::canonicalize(&newer).unwrap()]
            .into_iter()
            .collect::<HashSet<_>>();
        assert_eq!(
            latest_unclaimed_rollout_for_cwd_since(cwd.path(), base, dir.path(), &claimed)
                .as_deref(),
            Some(older.as_path()),
            "rehydrate must not bind two live Codex TUI sessions to the same rollout"
        );
    }

    #[test]
    fn maps_session_meta_assistant_text_tools_and_status() {
        let messages = collect_rollout(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"rollout-session","cwd":"/tmp/repo"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"date\"}","call_id":"call-1"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"Process exited with code 0\nOutput:\nSat"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello from rollout"}]}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"cached_input_tokens":3,"output_tokens":2}}}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"future_event","message":"ignored"}}"#,
                "\n",
            ),
            0,
        );

        assert!(matches!(
            &messages[0],
            StreamMessage::Init { session_id, .. } if session_id == "rollout-session"
        ));
        assert!(matches!(
            &messages[1],
            StreamMessage::ToolUse { name, input, .. }
                if name == "exec_command" && input.contains("\"cmd\":\"date\"")
        ));
        assert!(matches!(
            &messages[2],
            StreamMessage::ToolResult { content, is_error, .. }
                if content.contains("Process exited with code 0") && !is_error
        ));
        assert!(matches!(
            &messages[3],
            StreamMessage::Text { content } if content == "hello from rollout"
        ));
        assert!(matches!(
            &messages[4],
            StreamMessage::StatusUpdate {
                input_tokens: Some(4),
                cache_read_tokens: Some(3),
                output_tokens: Some(2),
                ..
            }
        ));
        match messages.last().expect("done message") {
            StreamMessage::Done { result, session_id } => {
                assert_eq!(result, "hello from rollout");
                assert_eq!(session_id.as_deref(), Some("rollout-session"));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn codex_token_count_context_uses_last_usage_not_total_usage() {
        let payload = serde_json::json!({
            "info": {
                "last_token_usage": {
                    "input_tokens": 117_600_u64,
                    "cached_input_tokens": 100_000_u64,
                    "output_tokens": 50_u64
                },
                "total_token_usage": {
                    "input_tokens": 2_300_000_u64,
                    "cached_input_tokens": 2_200_000_u64,
                    "output_tokens": 41_600_u64
                }
            }
        });

        match token_count_status(&payload).expect("last_token_usage should emit status") {
            StreamMessage::StatusUpdate {
                input_tokens,
                cache_read_tokens,
                output_tokens,
                ..
            } => {
                assert_eq!(input_tokens, Some(17_600));
                assert_eq!(cache_read_tokens, Some(100_000));
                assert_eq!(output_tokens, Some(50));
                let usage = crate::db::turns::TurnTokenUsage {
                    input_tokens: input_tokens.unwrap_or(0),
                    cache_create_tokens: 0,
                    cache_read_tokens: cache_read_tokens.unwrap_or(0),
                    output_tokens: output_tokens.unwrap_or(0),
                };
                assert_eq!(usage.context_occupancy_input_tokens(), 117_600);
            }
            other => panic!("expected StatusUpdate, got {other:?}"),
        }
    }

    #[test]
    fn codex_token_count_total_only_keeps_context_unknown() {
        let payload = serde_json::json!({
            "info": {
                "total_token_usage": {
                    "input_tokens": 2_300_000_u64,
                    "cached_input_tokens": 2_200_000_u64,
                    "output_tokens": 41_600_u64
                }
            }
        });

        match token_count_status(&payload).expect("total output can still emit telemetry") {
            StreamMessage::StatusUpdate {
                input_tokens,
                cache_read_tokens,
                output_tokens,
                ..
            } => {
                assert_eq!(input_tokens, None);
                assert_eq!(cache_read_tokens, None);
                assert_eq!(output_tokens, Some(41_600));
            }
            other => panic!("expected StatusUpdate, got {other:?}"),
        }
    }

    // U-8 fixture-level: a rollout with 5 paired tool_call / tool_result
    // entries followed by a single assistant message must produce a stable
    // sequence — every ToolUse emits, every ToolResult emits, and no
    // tool_call is dropped or duplicated by the parser. This is the
    // regression boundary for "tool_use 다발" relay scenarios.
    #[test]
    fn maps_five_tool_call_result_pairs_in_order() {
        let mut body = String::new();
        body.push_str(r#"{"type":"session_meta","payload":{"id":"five-tools","cwd":"/tmp/repo"}}"#);
        body.push('\n');
        for index in 0..5 {
            body.push_str(&format!(
                r#"{{"type":"response_item","payload":{{"type":"function_call","name":"tool_{index}","arguments":"{{\"i\":{index}}}","call_id":"c{index}"}}}}"#,
            ));
            body.push('\n');
            body.push_str(&format!(
                r#"{{"type":"response_item","payload":{{"type":"function_call_output","call_id":"c{index}","output":"out-{index}"}}}}"#,
            ));
            body.push('\n');
        }
        body.push_str(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"summary"}]}}"#,
        );
        body.push('\n');

        let messages = collect_rollout(&body, 0);

        // Strict ordered pair-interleave check: capture only the ToolUse,
        // ToolResult, and Text events in arrival order. The parser must
        // emit ToolUse(tool_i) immediately followed by ToolResult(out-i)
        // for each i in 0..5, with the assistant Text once at the end.
        // If a regression batches the 5 ToolUse before the 5 ToolResult
        // (or drops/duplicates pairs), this sequence comparison fails.
        let mut sequence: Vec<(&'static str, String)> = Vec::new();
        for message in &messages {
            match message {
                StreamMessage::ToolUse { name, .. } => sequence.push(("use", name.clone())),
                StreamMessage::ToolResult { content, .. } => {
                    sequence.push(("result", content.clone()))
                }
                StreamMessage::Text { content } => sequence.push(("text", content.clone())),
                _ => {}
            }
        }

        let mut expected: Vec<(&'static str, String)> = Vec::new();
        for i in 0..5 {
            expected.push(("use", format!("tool_{i}")));
            expected.push(("result", format!("out-{i}")));
        }
        expected.push(("text", "summary".to_string()));

        assert_eq!(
            sequence, expected,
            "tool_use/tool_result/text emit order regression"
        );
    }

    #[test]
    fn starts_at_known_offset_to_avoid_stale_replay() {
        let stale = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"stale"}]}}"#,
            "\n",
        );
        let fresh = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fresh"}]}}"#,
            "\n",
        );
        let messages = collect_rollout(&format!("{stale}{fresh}"), stale.len() as u64);

        assert!(messages.iter().all(
            |message| !matches!(message, StreamMessage::Text { content } if content == "stale")
        ));
        assert!(messages.iter().any(
            |message| matches!(message, StreamMessage::Text { content } if content == "fresh")
        ));
    }

    #[test]
    fn offset_tail_preserves_known_session_id_for_done() {
        let stale = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"stale"}]}}"#,
            "\n",
        );
        let fresh = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fresh"}]}}"#,
            "\n",
        );
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), format!("{stale}{fresh}")).unwrap();
        let (tx, rx) = mpsc::channel();

        tail_rollout_file_from_offset(
            file.path(),
            stale.len() as u64,
            Some("session-1"),
            tx,
            None,
            || false,
        )
        .unwrap();

        let messages = rx.iter().collect::<Vec<_>>();
        assert!(messages.iter().all(
            |message| !matches!(message, StreamMessage::Text { content } if content == "stale")
        ));
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, session_id })
                if result == "fresh" && session_id.as_deref() == Some("session-1")
        ));
    }

    #[test]
    fn resumed_tail_reads_prior_rollout_append_from_known_offset() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let stale = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"stale"}]}}"#,
            "\n",
        );
        let prior = write_rollout(
            dir.path(),
            "rollout-old.jsonl",
            "session-1",
            cwd.path(),
            stale,
        );
        let offset = std::fs::metadata(&prior).unwrap().len();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&prior)
            .unwrap()
            .write_all(
                concat!(
                    r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fresh append"}]}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();
        let (tx, rx) = mpsc::channel();

        tail_resumed_rollout_for_session_with_options(
            cwd.path(),
            "session-1",
            &prior,
            offset,
            SystemTime::now(),
            dir.path(),
            tx,
            None,
            || false,
            RolloutTailOptions {
                wait_for_rollout: Duration::from_millis(10),
                terminal_drain: Duration::ZERO,
                assistant_response_deadline: None,
                pending_tool_call_deadline: None,
                enable_task_complete_fast_path: true,
                legacy_terminal_drain: None,
                apply_pending_tool_deadline_without_assistant_text: true,
                tmux_session_name: None,
                discord_origin_prompt: None,
            },
        )
        .unwrap();

        let messages = rx.iter().collect::<Vec<_>>();
        assert!(messages.iter().all(
            |message| !matches!(message, StreamMessage::Text { content } if content == "stale")
        ));
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, session_id })
                if result == "fresh append" && session_id.as_deref() == Some("session-1")
        ));
    }

    #[test]
    fn resumed_tail_can_follow_new_rollout_for_same_session() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prior = write_rollout(
            dir.path(),
            "old/rollout-old.jsonl",
            "session-1",
            cwd.path(),
            "",
        );
        let offset = std::fs::metadata(&prior).unwrap().len();
        let modified_since = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        write_rollout(
            dir.path(),
            "new/rollout-new.jsonl",
            "session-1",
            cwd.path(),
            concat!(
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fresh new file"}]}}"#,
                "\n",
            ),
        );
        let (tx, rx) = mpsc::channel();

        tail_resumed_rollout_for_session_with_options(
            cwd.path(),
            "session-1",
            &prior,
            offset,
            modified_since,
            dir.path(),
            tx,
            None,
            || false,
            RolloutTailOptions {
                wait_for_rollout: Duration::from_millis(10),
                terminal_drain: Duration::ZERO,
                assistant_response_deadline: None,
                pending_tool_call_deadline: None,
                enable_task_complete_fast_path: true,
                legacy_terminal_drain: None,
                apply_pending_tool_deadline_without_assistant_text: true,
                tmux_session_name: None,
                discord_origin_prompt: None,
            },
        )
        .unwrap();

        let messages = rx.iter().collect::<Vec<_>>();
        assert!(matches!(
            messages.first(),
            Some(StreamMessage::Init { session_id, .. }) if session_id == "session-1"
        ));
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, session_id })
                if result == "fresh new file" && session_id.as_deref() == Some("session-1")
        ));
    }

    #[test]
    fn ignores_codex_exec_json_schema_in_rollout_adapter() {
        let messages = collect_rollout(
            concat!(
                r#"{"type":"thread.started","thread_id":"exec-thread"}"#,
                "\n",
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"exec text"}}"#,
                "\n",
                r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"rollout text"}]}}"#,
                "\n",
            ),
            0,
        );

        assert!(messages.iter().all(
            |message| !matches!(message, StreamMessage::Init { session_id, .. } if session_id == "exec-thread")
        ));
        assert!(messages.iter().all(
            |message| !matches!(message, StreamMessage::Text { content } if content == "exec text")
        ));
        assert!(messages.iter().any(
            |message| matches!(message, StreamMessage::Text { content } if content == "rollout text")
        ));
    }

    #[test]
    fn terminal_drain_processes_final_line_without_newline() {
        let messages = collect_rollout(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"final no newline"}]}}"#,
            0,
        );

        assert!(messages.iter().any(
            |message| matches!(message, StreamMessage::Text { content } if content == "final no newline")
        ));
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == "final no newline"
        ));
    }

    #[test]
    fn assistant_response_deadline_emits_timeout_done() {
        // #2182 follow-up: when the rollout stays at EOF past the deadline
        // and no assistant text has appeared, the tailer must emit a
        // terminal Done so the upstream turn unblocks.
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let rollout = write_rollout(
            dir.path(),
            "rollout-timeout.jsonl",
            "session-timeout",
            cwd.path(),
            "",
        );
        let (tx, rx) = mpsc::channel();
        let (result, _outcome) = tail_rollout_file_until_assistant_response(
            &rollout,
            0,
            None,
            &tx,
            None,
            || true, // pane stays alive — only the deadline rescues us
            Duration::ZERO,
            Some(Duration::from_millis(150)),
            None,
            true,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        drop(tx);
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        let done = messages
            .iter()
            .rev()
            .find(|message| matches!(message, StreamMessage::Done { .. }));
        assert!(matches!(
            done,
            Some(StreamMessage::Done { result, .. }) if result.contains("timed out")
        ));
    }

    /// #3419 B (activity-based idle): a turn that keeps emitting NON-assistant
    /// output (here `reasoning` lifecycle records) at an interval SHORTER than
    /// the idle deadline must NOT trip the no-assistant-response timeout — the
    /// idle clock resets on every real rollout record. Pre-B this measured
    /// `started_at.elapsed()` (absolute) and would have fired regardless of
    /// activity, killing a live long/interactive codex turn. We append several
    /// reasoning lines spaced under the deadline, then assert no timeout Done
    /// was emitted (and the turn ends only when the pane dies, not the timer).
    #[test]
    fn idle_deadline_survives_continuous_nonassistant_activity() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let rollout = write_rollout(
            dir.path(),
            "rollout-idle-active.jsonl",
            "session-idle-active",
            cwd.path(),
            "",
        );
        let (tx, rx) = mpsc::channel();
        // Pane death is the ONLY non-timer exit; flip it AFTER the activity
        // window so the test terminates deterministically without relying on
        // the (longer) idle deadline. `alive` stays true while we append.
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let writer_alive = alive.clone();
        let writer_path = rollout.clone();
        let writer = std::thread::spawn(move || {
            // 5 reasoning records, ~60ms apart (300ms total) — each one is a
            // rollout activity that resets the 200ms idle clock, so the gap
            // between activities (60ms) never reaches the deadline (200ms).
            for _ in 0..5 {
                std::thread::sleep(Duration::from_millis(60));
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&writer_path)
                    .unwrap();
                file.write_all(
                    b"{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\"}}\n",
                )
                .unwrap();
            }
            // Activity over: let the pane die so the tail exits via `is_alive`,
            // proving it reached EOF still ALIVE without the timer firing.
            std::thread::sleep(Duration::from_millis(60));
            writer_alive.store(false, std::sync::atomic::Ordering::SeqCst);
        });
        let is_alive_flag = alive.clone();
        let (result, _outcome) = tail_rollout_file_until_assistant_response(
            &rollout,
            0,
            None,
            &tx,
            None,
            move || is_alive_flag.load(std::sync::atomic::Ordering::SeqCst),
            Duration::ZERO,
            // Idle deadline (200ms) is shorter than the TOTAL active window
            // (~360ms) but longer than the per-activity gap (60ms): an
            // absolute-time gate would fire; an idle gate must not.
            Some(Duration::from_millis(200)),
            None,
            true,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        writer.join().unwrap();
        drop(tx);
        let messages: Vec<_> = rx.iter().collect();
        // No assistant text ever arrived, so the pane-death branch surfaces
        // SessionDied — NOT a timer-driven Completed/Done.
        assert!(
            matches!(result, ReadOutputResult::SessionDied { .. }),
            "live activity must keep the idle timer from firing; got {result:?}"
        );
        assert!(
            !messages.iter().any(|m| matches!(
                m,
                StreamMessage::Done { result, .. }
                    if result.contains("timed out") || result.contains("idle")
            )),
            "no idle/timeout Done may be emitted while activity is ongoing: {messages:?}"
        );
    }

    /// #3419 B: once output STOPS, the idle clock (measured from the last real
    /// record, not the tailer start) must elapse and fire the timeout Done.
    /// Here a single reasoning record arrives, then the rollout goes silent —
    /// after the idle window the no-assistant-response Done surfaces, which is
    /// what the downstream C finalizer drains. Crucially the deadline is timed
    /// from the LAST record: the early activity does not buy unlimited time.
    #[test]
    fn idle_deadline_fires_after_output_goes_silent() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let rollout = write_rollout(
            dir.path(),
            "rollout-idle-silent.jsonl",
            "session-idle-silent",
            cwd.path(),
            // One reasoning record up front, then permanent silence.
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\"}}\n",
        );
        // Append a second record after a short delay so `last_output_at` is set
        // well after `started_at`; the deadline must still fire promptly,
        // proving it tracks idle-since-last-record (not absolute age).
        let writer_path = rollout.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .unwrap();
            file.write_all(b"{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\"}}\n")
                .unwrap();
            // Then go silent forever — empty 100ms polls must NOT reset idle.
        });
        let (tx, rx) = mpsc::channel();
        let (result, _outcome) = tail_rollout_file_until_assistant_response(
            &rollout,
            0,
            None,
            &tx,
            None,
            || true, // pane stays ALIVE — only the idle deadline rescues us
            Duration::ZERO,
            Some(Duration::from_millis(150)),
            None,
            true,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        writer.join().unwrap();
        drop(tx);
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        let done = messages
            .iter()
            .rev()
            .find(|m| matches!(m, StreamMessage::Done { .. }));
        assert!(
            matches!(
                done,
                Some(StreamMessage::Done { result, .. }) if result.contains("timed out")
            ),
            "silence past the idle window must emit the timeout Done: {messages:?}"
        );
    }

    #[test]
    fn relay_suppression_drops_post_cancel_output() {
        // #2172 cancel boundary: once the cancel token flips, the rollout
        // tail MUST stop emitting StreamMessages — even for lines that were
        // already buffered or are written to the rollout AFTER cancel. The
        // bridge / Discord must not see a single post-cancel frame for the
        // cancelled turn.
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prefix = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"cancel-suppress\",\"cwd\":\"{}\"}}}}\n",
            cwd.path().display()
        );
        let rollout = dir.path().join("rollout-cancel.jsonl");
        std::fs::write(&rollout, &prefix).unwrap();

        let token = Arc::new(CancelToken::new());
        let (tx, rx) = mpsc::channel();

        // Spawn the tail. We cancel before any assistant text appears, then
        // append a "post_cancel" assistant message and verify it is never
        // delivered.
        let tail_token = token.clone();
        let tail_path = rollout.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &tail_path,
                0,
                None,
                &tx,
                Some(tail_token),
                || true,
                Duration::ZERO,
                Some(Duration::from_secs(5)),
                None,
                true,
                None,
                true,
                None,
                None,
            )
        });

        // Let the tail consume session_meta and reach the EOF wait.
        std::thread::sleep(Duration::from_millis(50));

        // Fire the cancel BEFORE writing more rollout content, so the tail
        // observes cancel in the wait loop AND the relay-suppression
        // sender drops the post-cancel append even if a race lets the read
        // pick it up.
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // Append a post-cancel assistant text that, without suppression,
        // would be relayed to Discord as part of the cancelled turn.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"post_cancel"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(
            matches!(result, ReadOutputResult::Cancelled { .. }),
            "tail must surface Cancelled after the token flips, got {:?}",
            result
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert!(
            messages.iter().all(|m| !matches!(
                m,
                StreamMessage::Text { content } if content.contains("post_cancel")
            )),
            "post-cancel rollout content must NOT be relayed; got messages={:?}",
            messages
        );
        assert!(
            messages
                .iter()
                .all(|m| !matches!(m, StreamMessage::Done { .. })),
            "post-cancel Done must NOT be relayed; got messages={:?}",
            messages
        );
    }

    #[test]
    fn two_segments_with_long_pause_emits_full_response() {
        // #2419 regression: codex CLI emits assistant text in
        // burst-pause-burst patterns. With the old 750ms drain a >1s inter-
        // segment silence caused the tailer to emit Done and shut down,
        // truncating Discord relay. With drain=2s the second segment must
        // still land in the same turn after a 1s pause.
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prefix = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"two-seg\",\"cwd\":\"{}\"}}}}\n",
            cwd.path().display()
        );
        let rollout = dir.path().join("rollout-two-seg.jsonl");
        std::fs::write(&rollout, &prefix).unwrap();
        // First segment is present from the start so the tail picks it up
        // and starts the drain countdown.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment1"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (tx, rx) = mpsc::channel();
        let tail_path = rollout.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &tail_path,
                0,
                None,
                &tx,
                None,
                || true,
                Duration::from_secs(2),
                Some(Duration::from_secs(10)),
                None,
                true,
                None,
                true,
                None,
                None,
            )
        });

        // Pause longer than the old 750ms default but shorter than the new
        // 2s drain used here. The tailer must NOT have finished yet.
        std::thread::sleep(Duration::from_millis(1100));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment2"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "segment1")),
            "segment1 must be emitted; got {:?}",
            messages
        );
        assert!(
            messages.iter().any(
                |m| matches!(m, StreamMessage::Text { content } if content.trim() == "segment2")
            ),
            "segment2 must be emitted after the inter-segment pause; got {:?}",
            messages
        );
        // Done's `result` accumulates both segments separated by blank line.
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. })
                if result.contains("segment1") && result.contains("segment2")
        ));
    }

    #[test]
    fn tool_call_pause_does_not_emit_premature_done() {
        // #2419: while a tool_call is in flight the assistant naturally goes
        // silent. The drain timer must be suppressed for that window so
        // segment2 (post-tool) still lands in the same turn.
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prefix = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"tool-pause\",\"cwd\":\"{}\"}}}}\n",
            cwd.path().display()
        );
        let rollout = dir.path().join("rollout-tool-pause.jsonl");
        std::fs::write(&rollout, &prefix).unwrap();
        // Pre-write: segment1 + function_call (no output yet). The drain
        // timer would normally trip on the ensuing silence — pending_tool_call
        // must suppress it.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment1"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}}
"#,
        )
        .unwrap();
        drop(file);

        let (tx, rx) = mpsc::channel();
        let tail_path = rollout.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &tail_path,
                0,
                None,
                &tx,
                None,
                || true,
                // Short drain — without the tool-call gate, this would fire
                // during the silence and emit Done before segment2 arrives.
                Duration::from_millis(150),
                Some(Duration::from_secs(10)),
                None,
                true,
                None,
                true,
                None,
                None,
            )
        });

        // Sleep long enough that drain WOULD have fired without gating.
        std::thread::sleep(Duration::from_millis(600));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment2"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        assert!(
            messages.iter().any(
                |m| matches!(m, StreamMessage::Text { content } if content.trim() == "segment2")
            ),
            "segment2 must be emitted after the tool call resolves; got {:?}",
            messages
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. })
                if result.contains("segment1") && result.contains("segment2")
        ));
    }

    #[test]
    fn tool_search_pause_does_not_emit_premature_done() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prefix = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"tool-search-pause\",\"cwd\":\"{}\"}}}}\n",
            cwd.path().display()
        );
        let rollout = dir.path().join("rollout-tool-search-pause.jsonl");
        std::fs::write(&rollout, &prefix).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment1"}]}}
{"type":"response_item","payload":{"type":"tool_search_call","call_id":"search-1","status":"in_progress"}}
"#,
        )
        .unwrap();
        drop(file);

        let (tx, rx) = mpsc::channel();
        let tail_path = rollout.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &tail_path,
                0,
                None,
                &tx,
                None,
                || true,
                Duration::from_millis(150),
                Some(Duration::from_secs(10)),
                None,
                true,
                None,
                true,
                None,
                None,
            )
        });

        std::thread::sleep(Duration::from_millis(600));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"tool_search_output","call_id":"search-1","status":"completed"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment2"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        assert!(
            messages.iter().any(
                |m| matches!(m, StreamMessage::Text { content } if content.trim() == "segment2")
            ),
            "segment2 must be emitted after the tool_search output resolves; got {:?}",
            messages
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. })
                if result.contains("segment1") && result.contains("segment2")
        ));
    }

    #[test]
    fn multiple_concurrent_tool_calls_keep_drain_gate_closed() {
        // #2419 (Codex review HIGH): two tool_call lines emitted before any
        // outputs arrive. The first matching output must NOT clear the
        // drain gate while the second call is still pending — otherwise
        // EOF + drain elapsed would emit Done before segment2.
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prefix = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"two-tool\",\"cwd\":\"{}\"}}}}\n",
            cwd.path().display()
        );
        let rollout = dir.path().join("rollout-two-tool.jsonl");
        std::fs::write(&rollout, &prefix).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment1"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"a","arguments":"{}","call_id":"c1"}}
{"type":"response_item","payload":{"type":"function_call","name":"b","arguments":"{}","call_id":"c2"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok-1"}}
"#,
        )
        .unwrap();
        drop(file);

        let (tx, rx) = mpsc::channel();
        let tail_path = rollout.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &tail_path,
                0,
                None,
                &tx,
                None,
                || true,
                Duration::from_millis(150),
                Some(Duration::from_secs(10)),
                None,
                true,
                None,
                true,
                None,
                None,
            )
        });

        // Long enough that drain WOULD fire if c2 were not still pending.
        std::thread::sleep(Duration::from_millis(600));

        // Now resolve c2 and append segment2 — both must land in the turn.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c2","output":"ok-2"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment2"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        assert!(
            messages.iter().any(
                |m| matches!(m, StreamMessage::Text { content } if content.trim() == "segment2")
            ),
            "segment2 must be emitted after BOTH concurrent tool calls resolve; got {:?}",
            messages
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. })
                if result.contains("segment1") && result.contains("segment2")
        ));
    }

    #[test]
    fn empty_tool_output_refreshes_drain_clock() {
        // #2419 (Codex review HIGH): a long-running tool call resolves with
        // an empty output (no StreamMessage emitted). Without lifecycle
        // refresh, `last_output_at` would still point at the original
        // tool_call timestamp, so the very next EOF tick would observe
        // elapsed > drain and emit Done before the post-tool assistant
        // text was appended. With refresh, the clock restarts at the empty
        // output and segment2 still lands in the same turn.
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prefix = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"empty-out\",\"cwd\":\"{}\"}}}}\n",
            cwd.path().display()
        );
        let rollout = dir.path().join("rollout-empty-out.jsonl");
        std::fs::write(&rollout, &prefix).unwrap();

        // Phase 1: segment1 + tool_call (tool now running, drain gate held
        // shut by pending_tool_calls).
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment1"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"silent","arguments":"{}","call_id":"c-empty"}}
"#,
        )
        .unwrap();
        drop(file);

        let (tx, rx) = mpsc::channel();
        let tail_path = rollout.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &tail_path,
                0,
                None,
                &tx,
                None,
                || true,
                Duration::from_millis(250),
                Some(Duration::from_secs(10)),
                None,
                true,
                None,
                true,
                None,
                None,
            )
        });

        // Phase 2: simulate a slow tool — silence for longer than drain.
        // pending_tool_call gate must keep drain suppressed here.
        std::thread::sleep(Duration::from_millis(400));

        // Phase 3: empty tool output arrives. No StreamMessage emitted.
        // Without lifecycle refresh, last_output_at still ≈ tool_call
        // timestamp (t≈0), and the very next EOF tick would fire Done.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c-empty","output":""}}
"#,
        )
        .unwrap();
        drop(file);

        // Phase 4: a short post-tool gap (< drain) — small enough that a
        // properly-refreshed clock has NOT yet elapsed. Then segment2.
        std::thread::sleep(Duration::from_millis(150));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment2"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        assert!(
            messages.iter().any(
                |m| matches!(m, StreamMessage::Text { content } if content.trim() == "segment2")
            ),
            "segment2 must land after empty tool output refreshes drain clock; got {:?}",
            messages
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. })
                if result.contains("segment1") && result.contains("segment2")
        ));
    }

    #[test]
    fn progress_event_msg_refreshes_drain_clock() {
        // #2477: Codex progress/subagent event_msg records may not produce a
        // visible StreamMessage. They still indicate the turn is active, so
        // they must refresh the drain clock between assistant bursts.
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prefix = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"progress-pause\",\"cwd\":\"{}\"}}}}\n",
            cwd.path().display()
        );
        let rollout = dir.path().join("rollout-progress-pause.jsonl");
        std::fs::write(&rollout, &prefix).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment1"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (tx, rx) = mpsc::channel();
        let tail_path = rollout.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &tail_path,
                0,
                None,
                &tx,
                None,
                || true,
                Duration::from_millis(300),
                Some(Duration::from_secs(10)),
                None,
                true,
                None,
                true,
                None,
                None,
            )
        });

        std::thread::sleep(Duration::from_millis(150));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"event_msg","payload":{"type":"exec_command_progress","message":"still running"}}
"#,
        )
        .unwrap();
        drop(file);

        std::thread::sleep(Duration::from_millis(200));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"segment2"}]}}
{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"segment1\n\nsegment2"}}
"#,
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        assert!(
            messages.iter().any(
                |m| matches!(m, StreamMessage::Text { content } if content.trim() == "segment2")
            ),
            "segment2 must be emitted after progress-only activity refreshes drain; got {:?}",
            messages
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. })
                if result.contains("segment1") && result.contains("segment2")
        ));
    }

    #[test]
    fn launch_discord_prompt_suppresses_late_rollout_user_message() {
        let tmux_session_name = format!("AgentDesk-codex-launch-dedupe-{}", std::process::id());
        let json = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "slow launch prompt" }
                ]
            }
        });
        let mut state = RolloutParseState {
            tmux_session_name: Some(tmux_session_name.clone()),
            discord_origin_prompt: Some("slow launch prompt".to_string()),
            ..RolloutParseState::default()
        };

        observe_rollout_user_prompt(&json, &mut state);

        assert!(state.discord_origin_prompt.is_none());
        assert_eq!(
            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux(
                "codex",
                &tmux_session_name,
                "slow launch prompt",
            ),
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedRecentDuplicate
        );
    }

    #[test]
    fn stuck_tool_call_deadline_emits_recovery_done() {
        // #2419 follow-up (Codex review HIGH round 2): assistant text was
        // already streamed, then a tool call was emitted but its output
        // never arrives (hung tool / mismatched call_id / schema skew).
        // The pane stays alive, so without a bounded recovery deadline the
        // tail would sleep forever and the Discord turn would hang. With
        // `pending_tool_call_deadline` the tail must surface a terminal
        // Done with a warning so the bridge advances.
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prefix = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"stuck-tool\",\"cwd\":\"{}\"}}}}\n",
            cwd.path().display()
        );
        let rollout = dir.path().join("rollout-stuck.jsonl");
        std::fs::write(&rollout, &prefix).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"never_returns","arguments":"{}","call_id":"c-stuck"}}
"#,
        )
        .unwrap();
        drop(file);

        let (tx, rx) = mpsc::channel();
        let (result, _outcome) = tail_rollout_file_until_assistant_response(
            &rollout,
            0,
            None,
            &tx,
            None,
            || true, // pane stays alive forever
            Duration::from_secs(60),
            Some(Duration::from_secs(60)),
            // Short bounded recovery deadline — without this the tail
            // would block forever.
            Some(Duration::from_millis(200)),
            true,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        drop(tx);

        assert!(
            matches!(result, ReadOutputResult::Completed { .. }),
            "tail must surface Completed once the pending-tool deadline expires; got {:?}",
            result
        );
        let messages: Vec<_> = rx.iter().collect();
        let done = messages
            .iter()
            .rev()
            .find(|m| matches!(m, StreamMessage::Done { .. }));
        assert!(
            matches!(
                done,
                Some(StreamMessage::Done { result, .. })
                    if result.contains("hello") && result.contains("did not resolve")
            ),
            "Done must contain prior assistant text and the recovery warning; got {:?}",
            messages
        );
    }

    #[test]
    fn preserves_multibyte_text_split_across_read_buffer_boundary() {
        let prefix = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":""#;
        let suffix = r#""}]}}"#;
        let fill_len = 8191usize.saturating_sub(prefix.as_bytes().len());
        let text = format!("{}가", "x".repeat(fill_len));
        let line = format!("{prefix}{text}{suffix}\n");

        assert_eq!(line.as_bytes()[8191], "가".as_bytes()[0]);

        let messages = collect_rollout(&line, 0);
        assert!(
            messages.iter().any(
                |message| matches!(message, StreamMessage::Text { content } if content == &text)
            )
        );
    }

    /// Helper that exercises the real read loop (not `replay_rollout_file`),
    /// so we can observe the EOF drain vs `task_complete` fast-path
    /// interaction. The `is_alive` closure flips to `false` once the rollout
    /// has been fully written, mirroring the production `pane_alive` signal.
    fn run_tail_with_options(
        body: &str,
        drain: Duration,
        deadline: Option<Duration>,
        enable_task_complete_fast_path: bool,
    ) -> (Vec<StreamMessage>, Duration) {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"task-complete-test\",\"cwd\":\"/tmp/repo\"}}}}\n{}",
                body
            ),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let alive = std::sync::atomic::AtomicBool::new(true);
        // pane stays alive for the entire read — only the fast-path / drain
        // should be able to terminate the tail. Once the read returns we
        // capture the elapsed wall time so the assertion can verify the
        // fast-path beat the drain.
        let (result, _outcome) = tail_rollout_file_until_assistant_response(
            file.path(),
            0,
            None,
            &tx,
            None,
            || alive.load(std::sync::atomic::Ordering::Relaxed),
            drain,
            deadline,
            None,
            enable_task_complete_fast_path,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        let elapsed = started.elapsed();
        drop(tx);
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        (rx.iter().collect(), elapsed)
    }

    #[test]
    fn commentary_phase_does_not_finalize_before_final_answer() {
        // Codex TUI can emit an intermediate assistant message with
        // `phase=commentary` long before the final answer. The EOF drain
        // must not treat that progress update as the final Done payload.
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"session_meta","payload":{"id":"commentary-final","cwd":"/tmp/repo"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"working first"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        let path = file.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &path,
                0,
                None,
                &tx,
                None,
                || true,
                Duration::from_millis(100),
                Some(Duration::from_secs(5)),
                None,
                true,
                None,
                true,
                None,
                None,
            )
        });

        std::thread::sleep(Duration::from_millis(350));
        let early_messages = rx.try_iter().collect::<Vec<_>>();
        assert!(
            early_messages
                .iter()
                .any(|message| matches!(message, StreamMessage::Text { content } if content == "working first")),
            "commentary text should still stream: {:?}",
            early_messages
        );
        assert!(
            early_messages
                .iter()
                .all(|message| !matches!(message, StreamMessage::Done { .. })),
            "commentary must not finalize the turn: {:?}",
            early_messages
        );

        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(file.path())
            .unwrap();
        writer
            .write_all(
                br#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"final answer"}]}}
{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"final answer"}}
"#,
            )
            .unwrap();
        drop(writer);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let remaining = rx.iter().collect::<Vec<_>>();
        // #3343 round 2: commentary is now part of BOTH the streamed surface and
        // `final_text` (the mirror property — the frozen consumer push_str's
        // every Text chunk, commentary included, into `full_response`). The
        // canonical body is therefore the boundary-joined commentary + final
        // answer, which is exactly what the streamed accumulation produces. The
        // test's core invariant — commentary must NOT finalize the turn before
        // the final answer arrives — is still asserted by the pre-final-answer
        // `early_messages` check above.
        assert!(
            matches!(
                remaining.last(),
                Some(StreamMessage::Done { result, .. }) if result == "working first\n\nfinal answer"
            ),
            "final Done must carry the boundary-joined commentary + final answer, got {:?}",
            remaining
        );
    }

    #[test]
    fn task_complete_emits_done_immediately_without_waiting_for_drain() {
        // #2423: when codex CLI emits `event_msg/task_complete`, the tail
        // must Done immediately. A 30s drain would normally pin the read
        // loop at EOF for ~30s before flushing Done; if the fast-path is
        // working we should finish within a fraction of that.
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"hello"}}"#,
            "\n",
        );
        let (messages, elapsed) = run_tail_with_options(
            body,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            true,
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "task_complete fast-path must short-circuit the 30s drain (took {:?})",
            elapsed
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == "hello"
        ));
    }

    #[test]
    fn task_complete_last_agent_message_promotes_full_body_over_streamed_tail() {
        let mut full_body = String::from("[E2E:E15:BEGIN]\n");
        for line in 1..=160 {
            full_body.push_str(&format!("E15-LINE-{line:03}\n"));
            if line == 80 {
                full_body.push_str("[E2E:E15:MID]\n");
            }
        }
        full_body.push_str("[E2E:E15:END]");
        let mut streamed_tail = String::new();
        for line in 150..=160 {
            streamed_tail.push_str(&format!("E15-LINE-{line:03}\n"));
        }
        streamed_tail.push_str("[E2E:E15:END]");
        let body = [
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": streamed_tail.clone()}]
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "turn_id": "t1",
                    "last_agent_message": full_body.clone()
                }
            })
            .to_string(),
        ]
        .join("\n")
            + "\n";

        let messages = collect_rollout(&body, 0);

        assert!(
            messages
                .iter()
                .any(|message| matches!(message, StreamMessage::Text { content } if content == &streamed_tail)),
            "streamed handoff should still expose the raw tail text event: {messages:?}"
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == &full_body
        ));
    }

    #[test]
    fn composer_ready_envelope_emits_done_before_drain() {
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ready by envelope"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"composer_ready"}}"#,
            "\n",
        );
        let (messages, elapsed) = run_tail_with_options(
            body,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            true,
        );

        assert!(
            elapsed < Duration::from_secs(2),
            "composer_ready envelope must short-circuit the 30s drain (took {:?})",
            elapsed
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == "ready by envelope"
        ));
    }

    #[tokio::test]
    async fn codex_stop_hook_emits_done_before_drain() {
        use axum::body::Body;
        use axum::http::{Method, Request};
        use tower::ServiceExt;

        let session = format!(
            "agentdesk-codex-rollout-hook-done-{}",
            uuid::Uuid::new_v4().simple()
        );
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ready by hook"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        let path = file.path().to_path_buf();
        let started = Instant::now();
        let tail_session = session.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &path,
                0,
                None,
                &tx,
                None,
                || true,
                Duration::from_secs(30),
                Some(Duration::from_secs(60)),
                None,
                true,
                None,
                true,
                Some(tail_session),
                None,
            )
        });

        std::thread::sleep(Duration::from_millis(250));
        let app = crate::services::claude_tui::hook_server::hook_receiver_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/hooks/codex/Stop?session_id={session}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"hook_event_name":"Stop"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "Codex Stop hook must short-circuit the 30s drain (took {:?})",
            elapsed
        );

        let messages = rx.iter().collect::<Vec<_>>();
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == "ready by hook"
        ));
    }

    #[test]
    fn task_complete_waits_for_pending_tool_call_output() {
        // #2423: codex may emit `task_complete` while the matching
        // `function_call_output` line is still buffered. The
        // `pending_tool_call_depth` guard must hold the fast-path until
        // the output is observed so the relay does not drop the final
        // tool result.
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"date\"}","call_id":"call-1"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"after tool"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"final tool output"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"after tool"}]}}"#,
            "\n",
        );
        // Use a long drain so the fast-path is the only thing that can
        // terminate the tail in a reasonable amount of time.
        let (messages, elapsed) = run_tail_with_options(
            body,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            true,
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "fast-path should fire once tool depth drains (took {:?})",
            elapsed
        );

        // Final tool output MUST have been relayed before Done.
        let tool_result_idx = messages
            .iter()
            .position(|m| matches!(m, StreamMessage::ToolResult { content, .. } if content.contains("final tool output")));
        let done_idx = messages
            .iter()
            .position(|m| matches!(m, StreamMessage::Done { .. }));
        assert!(
            tool_result_idx.is_some(),
            "final ToolResult must be in the stream: {:?}",
            messages
        );
        assert!(done_idx.is_some(), "Done must be emitted: {:?}", messages);
        assert!(
            tool_result_idx.unwrap() < done_idx.unwrap(),
            "ToolResult MUST precede Done (got {:?})",
            messages
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == "after tool"
        ));
    }

    #[test]
    fn task_complete_without_assistant_text_uses_last_agent_message() {
        // #2423: tool-only turns (or rollouts that carry the final answer
        // only on the `task_complete.last_agent_message` field) should
        // produce a `Done.result` populated from the fallback string —
        // empty Done would lose the response.
        let body = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"answer via last_agent_message"}}"#,
            "\n",
        );
        let (messages, _elapsed) = run_tail_with_options(
            body,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            true,
        );
        assert!(
            matches!(
                messages.last(),
                Some(StreamMessage::Done { result, .. })
                    if result == "answer via last_agent_message"
            ),
            "expected Done with last_agent_message fallback, got {:?}",
            messages
        );
    }

    #[test]
    fn task_complete_synthesizes_composer_ready_for_tmux_session() {
        let session = format!(
            "agentdesk-codex-rollout-composer-ready-{}",
            uuid::Uuid::new_v4().simple()
        );
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ready"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"ready"}}"#,
            "\n",
        );
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), body).unwrap();
        let (tx, _rx) = mpsc::channel();

        tail_rollout_file_until_assistant_response(
            file.path(),
            0,
            None,
            &tx,
            None,
            || false,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            None,
            true,
            None,
            true,
            Some(session.clone()),
            None,
        )
        .unwrap();

        assert!(
            crate::services::codex_tui::input::wait_until_codex_tui_input_ready(
                &session,
                crate::services::codex_tui::input::PromptReadinessKind::PostTurnHandoff,
                None,
            )
            .is_ok(),
            "synthetic composer_ready should release readiness without capture-pane"
        );
    }

    #[test]
    fn item_completed_agent_message_synthesizes_composer_ready_without_relaying_text() {
        let session = format!(
            "agentdesk-codex-rollout-item-ready-{}",
            uuid::Uuid::new_v4().simple()
        );
        let body = concat!(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"exec text"}}"#,
            "\n",
        );
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), body).unwrap();
        let (tx, rx) = mpsc::channel();

        tail_rollout_file_until_assistant_response(
            file.path(),
            0,
            None,
            &tx,
            None,
            || false,
            Duration::ZERO,
            None,
            None,
            true,
            None,
            true,
            Some(session.clone()),
            None,
        )
        .unwrap();

        let messages = rx.try_iter().collect::<Vec<_>>();
        assert!(
            messages
                .iter()
                .all(|message| !matches!(message, StreamMessage::Text { content } if content == "exec text")),
            "item.completed agent_message remains a readiness hint, not relayed TUI text: {:?}",
            messages
        );
        assert!(
            crate::services::codex_tui::input::wait_until_codex_tui_input_ready(
                &session,
                crate::services::codex_tui::input::PromptReadinessKind::PostTurnHandoff,
                None,
            )
            .is_ok(),
            "item.completed agent_message should release readiness without capture-pane"
        );
    }

    #[test]
    fn missing_task_complete_falls_back_to_drain() {
        // #2423: legacy Codex CLI versions / rollouts without
        // `event_msg/task_complete` MUST still terminate via the
        // `terminal_drain` safety net so the tail does not hang
        // indefinitely. This is the same path #2422 ships on; we only
        // want to ensure the fast-path additions did not regress it.
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"legacy"}]}}"#,
            "\n",
        );
        let (messages, elapsed) = run_tail_with_options(
            body,
            Duration::from_millis(150),
            Some(Duration::from_secs(60)),
            true,
        );
        // Drain must fire approximately within `drain` (with read-poll
        // jitter). A loose upper bound of 2s keeps the test stable on CI.
        assert!(
            elapsed >= Duration::from_millis(100),
            "drain must wait for the terminal window (took {:?})",
            elapsed
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "drain should fire close to its configured value (took {:?})",
            elapsed
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == "legacy"
        ));
    }

    #[test]
    fn disabling_fast_path_keeps_legacy_drain_behaviour() {
        // #2423: the `enable_task_complete_fast_path` escape hatch must
        // restore the pre-#2423 behaviour — `task_complete` becomes a
        // no-op and termination is decided solely by the drain. This
        // protects an operations rollback path if a future Codex CLI
        // ships a malformed `task_complete` event.
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"with disabled fast path"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"with disabled fast path"}}"#,
            "\n",
        );
        let (messages, elapsed) = run_tail_with_options(
            body,
            Duration::from_millis(200),
            Some(Duration::from_secs(60)),
            false,
        );
        assert!(
            elapsed >= Duration::from_millis(150),
            "with fast-path disabled the drain must be the gating timer (took {:?})",
            elapsed
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == "with disabled fast path"
        ));
    }

    #[test]
    fn task_complete_recorded_but_pending_tool_call_blocks_fast_path() {
        // #2423: explicit white-box check of the depth guard.
        // task_complete arrives while a function_call is still
        // outstanding → fast-path must wait. We verify by giving a
        // very long drain and asserting that, until the
        // function_call_output line appears, the tail does NOT
        // terminate. Since reaching the drain itself would take 30s+,
        // we instead append the output after a delay.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-pending.jsonl");
        let initial = concat!(
            r#"{"type":"session_meta","payload":{"id":"pending-test","cwd":"/tmp/repo"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"date\"}","call_id":"call-1"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"after tool"}}"#,
            "\n",
        );
        std::fs::write(&rollout, initial).unwrap();
        let (tx, rx) = mpsc::channel();
        let path = rollout.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &path,
                0,
                None,
                &tx,
                None,
                || true,
                Duration::from_secs(30),
                Some(Duration::from_secs(60)),
                None,
                true,
                None,
                true,
                None,
                None,
            )
        });
        // Give the tail enough time to ingest the pre-output entries and
        // reach EOF. The fast-path MUST NOT fire here.
        std::thread::sleep(Duration::from_millis(300));
        // Append the matching function_call_output + assistant text.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            concat!(
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"after tool"}]}}"#,
                "\n",
            )
            .as_bytes(),
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == "after tool"
        ));
    }

    #[test]
    fn legacy_cli_long_pause_does_not_emit_premature_done() {
        // #2453: a legacy Codex CLI (no `event_msg` records at all) emits
        // assistant text in burst-pause-burst. With the old uniform 5s
        // terminal drain a >5s pause would Done after burst1, truncating
        // burst2. The `legacy_terminal_drain` bump (15s default) must
        // absorb a 6s inter-burst pause and let burst2 land in the same
        // turn.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let prefix = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"legacy-burst\",\"cwd\":\"{}\"}}}}\n",
            cwd.path().display()
        );
        let rollout = dir.path().join("rollout-legacy-burst.jsonl");
        std::fs::write(&rollout, &prefix).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"burst1"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (tx, rx) = mpsc::channel();
        let tail_path = rollout.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &tail_path,
                0,
                None,
                &tx,
                None,
                || true,
                // Base drain stays 1s (legacy CLIs still need a *short* base
                // so the test runs fast). The legacy override (3s) is what
                // matters here — without #2453 the inter-burst silence of
                // 1.5s would fire the drain prematurely.
                Duration::from_secs(1),
                Some(Duration::from_secs(30)),
                None,
                true,
                Some(Duration::from_secs(3)),
                true,
                None,
                None,
            )
        });

        // Sleep 1.5s — past the base 1s drain but well inside the legacy 3s
        // drain. Without #2453, burst1's Done would already have fired.
        std::thread::sleep(Duration::from_millis(1500));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"burst2"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "burst1")),
            "burst1 must be emitted; got {:?}",
            messages
        );
        assert!(
            messages.iter().any(
                |m| matches!(m, StreamMessage::Text { content } if content.trim() == "burst2")
            ),
            "burst2 must be emitted after the legacy-drain-protected pause; got {:?}",
            messages
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. })
                if result.contains("burst1") && result.contains("burst2")
        ));
    }

    #[test]
    fn modern_cli_keeps_base_drain_when_event_msg_seen() {
        // #2453 dual: when the rollout DOES carry an `event_msg` record
        // (modern CLI fingerprint), the tail must NOT apply the legacy
        // drain bump — modern CLIs rely on the shorter base drain so a
        // Done that arrives without `task_complete` (e.g. token_count
        // only) still flushes promptly.
        let body = concat!(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1,"output_tokens":1}}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"modern"}]}}"#,
            "\n",
        );
        let (messages, elapsed) = run_tail_with_options(
            body,
            Duration::from_millis(250),
            Some(Duration::from_secs(30)),
            // Disable the task_complete fast-path so termination MUST come
            // from the drain. If the legacy bump leaked in here, this test
            // would take 10s+.
            false,
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "modern CLI with event_msg must use base drain, not legacy bump (took {:?})",
            elapsed
        );
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, .. }) if result == "modern"
        ));
    }

    #[test]
    fn modern_tmux_heuristic_waits_for_explicit_completion() {
        let mut modern_tmux = RolloutParseState {
            tmux_session_name: Some("agentdesk-codex-modern".to_string()),
            seen_any_event_msg: true,
            ..RolloutParseState::default()
        };
        assert!(!heuristic_finalize_allowed(
            &mut modern_tmux,
            Path::new("rollout-modern.jsonl"),
            42
        ));
        assert!(modern_tmux.heuristic_finalize_waiting_for_completion_logged);

        let mut legacy_tmux = RolloutParseState {
            tmux_session_name: Some("agentdesk-codex-legacy".to_string()),
            seen_any_event_msg: false,
            ..RolloutParseState::default()
        };
        assert!(heuristic_finalize_allowed(
            &mut legacy_tmux,
            Path::new("rollout-legacy.jsonl"),
            42
        ));

        let mut replay = RolloutParseState {
            seen_any_event_msg: true,
            ..RolloutParseState::default()
        };
        assert!(heuristic_finalize_allowed(
            &mut replay,
            Path::new("rollout-replay.jsonl"),
            42
        ));
    }

    #[cfg(unix)]
    #[test]
    fn modern_tmux_rollout_does_not_done_while_pane_still_working() {
        if !std::process::Command::new("tmux")
            .arg("-V")
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }

        let session = format!(
            "agentdesk-codex-rollout-busy-{}",
            uuid::Uuid::new_v4().simple()
        );
        let started = std::process::Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session,
                "printf 'Working\\n'; sleep 60",
            ])
            .status()
            .is_ok_and(|status| status.success());
        if !started {
            return;
        }

        struct KillTmuxSession(String);
        impl Drop for KillTmuxSession {
            fn drop(&mut self) {
                let _ = std::process::Command::new("tmux")
                    .args(["kill-session", "-t", &self.0])
                    .status();
            }
        }
        let _guard = KillTmuxSession(session.clone());

        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-modern-busy.jsonl");
        std::fs::write(
            &rollout,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"modern-busy","cwd":"/tmp/repo"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1,"output_tokens":1}}}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"progress only"}]}}"#,
                "\n",
            ),
        )
        .unwrap();

        let (tx, rx) = mpsc::channel();
        let path = rollout.clone();
        let tail_session = session.clone();
        let handle = std::thread::spawn(move || {
            tail_rollout_file_until_assistant_response(
                &path,
                0,
                None,
                &tx,
                None,
                || true,
                Duration::from_millis(100),
                Some(Duration::from_secs(30)),
                None,
                true,
                None,
                true,
                Some(tail_session),
                None,
            )
        });

        std::thread::sleep(Duration::from_millis(450));
        assert!(
            !handle.is_finished(),
            "modern tmux tail must not emit heuristic Done while the pane is still busy"
        );
        assert!(
            rx.try_iter()
                .all(|message| !matches!(message, StreamMessage::Done { .. })),
            "busy-pane drain must not emit Done before an explicit completion signal"
        );

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(
            br#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"progress only"}}
"#,
        )
        .unwrap();
        drop(file);

        let (result, _outcome) = handle.join().unwrap().unwrap();
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
    }

    #[test]
    fn tool_first_pending_call_deadline_fires_without_assistant_text() {
        // #2429 HIGH 2: a `function_call` emitted BEFORE any assistant
        // text that never resolves used to fall back to the 30 min
        // assistant_response_deadline. With
        // `apply_pending_tool_deadline_without_assistant_text=true` (the
        // default) the bounded pending-tool deadline now fires, surfacing
        // a tool-first warning so the bridge can advance.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-tool-first.jsonl");
        let initial = concat!(
            r#"{"type":"session_meta","payload":{"id":"tool-first","cwd":"/tmp/repo"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"hang","arguments":"{}","call_id":"hang-1"}}"#,
            "\n",
        );
        std::fs::write(&rollout, initial).unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        writer.write_all(b"").unwrap();
        drop(writer);

        let (tx, rx) = mpsc::channel();
        let (result, _outcome) = tail_rollout_file_until_assistant_response(
            &rollout,
            0,
            None,
            &tx,
            None,
            || true, // pane stays alive — only the deadline rescues us
            Duration::from_secs(60),
            // Long global deadline so we KNOW the tool-call deadline is
            // what fires, not the assistant-response deadline.
            Some(Duration::from_secs(60)),
            // Short bounded recovery deadline.
            Some(Duration::from_millis(200)),
            true,
            None,
            // The behaviour under test.
            true,
            None,
            None,
        )
        .unwrap();
        drop(tx);
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        let done = messages
            .iter()
            .find_map(|m| {
                if let StreamMessage::Done { result, .. } = m {
                    Some(result.as_str())
                } else {
                    None
                }
            })
            .expect("tool-first Done must be emitted by the pending-tool deadline");
        assert!(
            done.contains("before any assistant text"),
            "tool-first Done should carry the no-text warning copy; got {:?}",
            done
        );
    }

    #[test]
    fn tool_first_pending_call_deadline_can_be_disabled() {
        // #2429 HIGH 2 escape hatch: if an operator wants the pre-#2429
        // behaviour (deadline only after assistant text), setting the
        // flag to `false` must restore it — the tool-first hang stays
        // pinned by the pending-call gate and only the global
        // assistant_response_deadline can rescue it.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-tool-first-disabled.jsonl");
        let initial = concat!(
            r#"{"type":"session_meta","payload":{"id":"tool-first-off","cwd":"/tmp/repo"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"hang","arguments":"{}","call_id":"hang-1"}}"#,
            "\n",
        );
        std::fs::write(&rollout, initial).unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        writer.write_all(b"").unwrap();
        drop(writer);

        let (tx, rx) = mpsc::channel();
        let (result, _outcome) = tail_rollout_file_until_assistant_response(
            &rollout,
            0,
            None,
            &tx,
            None,
            || true,
            Duration::from_secs(60),
            // The global deadline is the only rescue path with the flag off.
            Some(Duration::from_millis(400)),
            // Short pending-tool deadline; would fire first if the flag
            // were on. With the flag off the tail must ignore it.
            Some(Duration::from_millis(100)),
            true,
            None,
            // The behaviour under test.
            false,
            None,
            None,
        )
        .unwrap();
        drop(tx);
        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = rx.iter().collect();
        let done = messages
            .iter()
            .find_map(|m| {
                if let StreamMessage::Done { result, .. } = m {
                    Some(result.as_str())
                } else {
                    None
                }
            })
            .expect("global assistant-response deadline must rescue the turn");
        assert!(
            done.contains("no assistant response"),
            "with the flag off the global assistant-response deadline copy must surface; got {:?}",
            done
        );
    }

    // #3343: collect the streamed text chunks in emission order so the
    // boundary-separator assertions read the surface a frozen
    // watcher/bridge `push_str(&content)` consumer would concatenate.
    fn streamed_text_chunks(messages: &[StreamMessage]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|m| match m {
                StreamMessage::Text { content } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    // #3343 (1) — pins the bug. Two DISTINCT assistant `response_item/message`
    // records (the real Codex rollout shape) must NOT collapse into a
    // `보겠습니다.로그` wall when their `StreamMessage::Text` content is appended
    // with a raw `push_str`. The second chunk must carry a `\n\n` boundary so
    // the joined surface separates the two messages. FAILS on base, where the
    // emitted Text is the raw `text` with no separator.
    #[test]
    fn distinct_codex_messages_join_with_paragraph_boundary() {
        let messages = collect_rollout(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"sep-1","cwd":"/tmp/repo"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"로그 꼬리를 확인해 보겠습니다."}]}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"release workspace 정리를 진행합니다."}]}}"#,
                "\n",
            ),
            0,
        );
        let chunks = streamed_text_chunks(&messages);
        assert_eq!(chunks.len(), 2, "two distinct messages -> two chunks");
        assert_eq!(chunks[0], "로그 꼬리를 확인해 보겠습니다.");
        // The boundary travels with the second chunk so the frozen consumer's
        // raw push_str renders readable separation.
        assert_eq!(chunks[1], "\n\nrelease workspace 정리를 진행합니다.");

        let joined = chunks.concat();
        assert!(
            joined.contains("보겠습니다.\n\nrelease workspace"),
            "distinct Codex messages must keep a paragraph boundary, not \
             collapse into `보겠습니다.release`; got {joined:?}"
        );
        assert!(
            !joined.contains("보겠습니다.release"),
            "the unseparated `문장.다음문장` wall must not appear; got {joined:?}"
        );
    }

    // #3343 (2) — a single assistant message streamed as the only record must
    // be emitted untouched: no leading separator is injected before the first
    // chunk of the turn, so we never break inside a single message.
    #[test]
    fn single_codex_message_streams_without_injected_separator() {
        let messages = collect_rollout(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"sep-2","cwd":"/tmp/repo"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"단일 메시지입니다."}]}}"#,
                "\n",
            ),
            0,
        );
        let chunks = streamed_text_chunks(&messages);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0], "단일 메시지입니다.",
            "the sole message must stream verbatim, never gaining a leading \\n\\n"
        );
    }

    // #3343 (3) — when a message already ends with a newline (or the next
    // starts with one) the boundary join must NOT double the separator into
    // `\n\n\n`. The helper is the single source of truth for the boundary
    // decision, so assert it directly across the no-double cases.
    #[test]
    fn message_boundary_join_never_doubles_existing_separator() {
        // First chunk of the turn: emitted untouched.
        assert_eq!(
            join_streamed_message_boundary(None, "first message"),
            "first message"
        );
        // Previous chunk already ended with a newline -> no extra separator.
        assert_eq!(
            join_streamed_message_boundary(Some(true), "next message"),
            "next message"
        );
        // New chunk already starts with a newline -> no extra separator.
        assert_eq!(
            join_streamed_message_boundary(Some(false), "\nnext message"),
            "\nnext message"
        );
        // Both sides mid-prose -> exactly one `\n\n` boundary, never `\n\n\n`.
        let joined = join_streamed_message_boundary(Some(false), "next message");
        assert_eq!(joined, "\n\nnext message");
        assert!(
            !joined.contains("\n\n\n"),
            "boundary join must never produce a tripled separator; got {joined:?}"
        );
    }

    // #3343 — end-to-end across the parser: a turn that ends with a message
    // already carrying a trailing newline must not double when the next
    // message arrives, and final_text stays consistent with the streamed
    // surface (both joined with a single `\n\n`).
    #[test]
    fn trailing_newline_message_does_not_double_at_boundary() {
        let messages = collect_rollout(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"sep-3","cwd":"/tmp/repo"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"첫 메시지\n"}]}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"둘째 메시지"}]}}"#,
                "\n",
            ),
            0,
        );
        let chunks = streamed_text_chunks(&messages);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "첫 메시지\n");
        // Prior chunk ended on a newline, so only a single \n separates them —
        // never \n\n\n.
        assert_eq!(chunks[1], "둘째 메시지");
        let joined = chunks.concat();
        assert!(
            !joined.contains("\n\n\n"),
            "must not double an existing trailing newline; got {joined:?}"
        );
    }

    // #3343 round 2 — extract the terminal `Done.result` (the parser's
    // assembled `final_text`) so the mirror assertion can compare it against the
    // streamed chunk accumulation. In replay (`terminal_drain == 0`) a turn with
    // assistant text and no pending tool emits the heuristic Done carrying
    // `state.final_text`.
    fn done_result(messages: &[StreamMessage]) -> Option<String> {
        messages.iter().find_map(|m| match m {
            StreamMessage::Done { result, .. } => Some(result.clone()),
            _ => None,
        })
    }

    // #3343 round 2 (1) — commentary boundary pin. Two DISTINCT
    // `phase:"commentary"` records must NOT collapse into a `문장.다음문장` wall:
    // the second commentary chunk must carry the `\n\n` boundary. FAILS on HEAD
    // 1757e7520, where commentary text bypassed `join_streamed_message_boundary`
    // entirely and streamed as the raw `text` with no separator.
    #[test]
    fn distinct_commentary_records_join_with_paragraph_boundary() {
        let messages = collect_rollout(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"comm-1","cwd":"/tmp/repo"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"로그를 살펴보는 중입니다."}]}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"workspace 상태를 확인합니다."}]}}"#,
                "\n",
            ),
            0,
        );
        let chunks = streamed_text_chunks(&messages);
        assert_eq!(chunks.len(), 2, "two commentary records -> two chunks");
        assert_eq!(chunks[0], "로그를 살펴보는 중입니다.");
        assert_eq!(
            chunks[1], "\n\nworkspace 상태를 확인합니다.",
            "the second commentary chunk must carry the \\n\\n boundary"
        );
        let joined = chunks.concat();
        assert!(
            joined.contains("중입니다.\n\nworkspace"),
            "distinct commentary records must keep a paragraph boundary; got {joined:?}"
        );
        assert!(
            !joined.contains("중입니다.workspace"),
            "commentary must not collapse into a `문장.다음문장` wall; got {joined:?}"
        );
    }

    // #3343 round 2 (2) — mixed phases: a commentary record followed by an
    // assistant (final) record must keep the `\n\n` boundary between them, and
    // the newline witness must carry across the phase change so the assistant
    // chunk picks up the boundary from the prior commentary chunk. FAILS on
    // HEAD: commentary did not advance the witness, so the following assistant
    // chunk saw `None` and streamed without a separator.
    #[test]
    fn mixed_commentary_then_assistant_keeps_boundary() {
        let messages = collect_rollout(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"comm-2","cwd":"/tmp/repo"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"먼저 빌드를 돌립니다."}]}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"빌드가 통과했습니다."}]}}"#,
                "\n",
            ),
            0,
        );
        let chunks = streamed_text_chunks(&messages);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "먼저 빌드를 돌립니다.");
        assert_eq!(
            chunks[1], "\n\n빌드가 통과했습니다.",
            "the assistant chunk must inherit the boundary from the prior commentary chunk"
        );
        // Witness consistency: final_text (Done.result) must equal the streamed
        // accumulation — commentary is now part of both surfaces.
        let final_text = done_result(&messages).expect("turn must finalize with Done");
        assert_eq!(
            final_text,
            chunks.concat(),
            "final_text must mirror the streamed accumulation across phases"
        );
    }

    // #3343 r2 review P2 — commentary-only turn whose `task_complete` carries
    // the SAME body as the mirrored commentary must not duplicate it in
    // `final_text`. FAILS on 26fa75fd4: the `!saw_assistant_text` append path
    // ignored the already-mirrored commentary and appended the fallback again.
    #[test]
    fn commentary_only_task_complete_does_not_duplicate_body() {
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"중간 점검 코멘트입니다."}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"중간 점검 코멘트입니다."}}"#,
            "\n",
        );
        let (messages, _elapsed) = run_tail_with_options(
            body,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            true,
        );
        let Some(StreamMessage::Done { result, .. }) = messages.last() else {
            panic!("turn must finalize with Done, got {messages:?}");
        };
        assert_eq!(
            result.matches("중간 점검 코멘트입니다.").count(),
            1,
            "commentary-only task_complete must carry the body exactly once: {result:?}"
        );
        // Superset fallback still supersedes the mirrored commentary (replace,
        // not append): the authoritative terminal body wins without doubling.
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"코멘트 본문."}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"서문.\n\n코멘트 본문."}}"#,
            "\n",
        );
        let (messages, _elapsed) = run_tail_with_options(
            body,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            true,
        );
        let Some(StreamMessage::Done { result, .. }) = messages.last() else {
            panic!("turn must finalize with Done, got {messages:?}");
        };
        assert_eq!(
            result.matches("코멘트 본문.").count(),
            1,
            "superseding fallback must replace, never double: {result:?}"
        );
        assert!(result.starts_with("서문."), "{result:?}");
    }

    // #3343 r3 review P2 — a short canonical terminal body that appears only
    // as a mid-sentence SUBSTRING of the mirrored commentary is genuinely new
    // and must append, not drop. FAILS on the r2 fix: arbitrary
    // `contains(text.trim())` treated the embedded quote as already mirrored
    // and finalized on the commentary alone.
    #[test]
    fn task_complete_substring_of_commentary_still_lands() {
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"검증이 끝나면 VERDICT: CLEAN 으로 보고하겠습니다."}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"VERDICT: CLEAN"}}"#,
            "\n",
        );
        let (messages, _elapsed) = run_tail_with_options(
            body,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            true,
        );
        let Some(StreamMessage::Done { result, .. }) = messages.last() else {
            panic!("turn must finalize with Done, got {messages:?}");
        };
        assert!(
            result.trim_end().ends_with("VERDICT: CLEAN"),
            "terminal body embedded in commentary must still append: {result:?}"
        );
        // The mirrored-equality drop still holds: a fallback that ends the
        // accumulated text at a message boundary lands exactly once.
        assert_eq!(result.matches("검증이 끝나면").count(), 1, "{result:?}");

        // r4 P3 — an INDENTED mirrored body is still "already mirrored": the
        // boundary check tolerates horizontal whitespace after the newline.
        let body = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"상태 보고.\n\n  VERDICT: CLEAN"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"VERDICT: CLEAN"}}"#,
            "\n",
        );
        let (messages, _elapsed) = run_tail_with_options(
            body,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            true,
        );
        let Some(StreamMessage::Done { result, .. }) = messages.last() else {
            panic!("turn must finalize with Done, got {messages:?}");
        };
        assert_eq!(
            result.matches("VERDICT: CLEAN").count(),
            1,
            "indented mirrored suffix must drop, not double: {result:?}"
        );
    }

    // #3343 round 2 (3) — mirror property. For a multi-record fixture mixing
    // commentary, assistant, and a trailing-newline record, the streamed chunk
    // accumulation must equal EXACTLY the `final_text` the parser assembles
    // (the `Done.result`). FAILS on HEAD: the streamed path suppressed the
    // separator on an existing newline while `final_text` always inserted
    // `\n\n`, and commentary was streamed but never written to `final_text` —
    // both diverge the two surfaces.
    #[test]
    fn streamed_accumulation_mirrors_final_text() {
        let messages = collect_rollout(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"mirror-1","cwd":"/tmp/repo"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"진행 상황을 보고합니다."}]}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"첫째 단락입니다.\n"}]}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"둘째 단락입니다."}]}}"#,
                "\n",
            ),
            0,
        );
        let chunks = streamed_text_chunks(&messages);
        let streamed_accumulation = chunks.concat();
        let final_text = done_result(&messages).expect("turn must finalize with Done");
        assert_eq!(
            streamed_accumulation, final_text,
            "streamed chunk accumulation must equal the assembled final_text \
             (mirror property); streamed={streamed_accumulation:?} final={final_text:?}"
        );
        // The trailing-newline record must not be doubled into `\n\n\n` on
        // either surface (canonical suppress-on-existing-newline rule).
        assert!(
            !final_text.contains("\n\n\n"),
            "the unified boundary must never produce a tripled separator; got {final_text:?}"
        );
        // Sanity: distinct records stay visually separated, not walled.
        assert!(
            final_text.contains("보고합니다.\n\n첫째 단락입니다.\n둘째 단락입니다."),
            "records must keep their boundaries; got {final_text:?}"
        );
    }
}
