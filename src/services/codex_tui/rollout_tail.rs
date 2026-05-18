use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::{CancelToken, ReadOutputResult, cancel_requested};

const DEFAULT_ROLLOUT_WAIT_SECS: u64 = 30;
/// Fallback EOF drain budget for rollouts that do NOT emit an explicit
/// `event_msg/task_complete` signal (legacy Codex CLI versions or unexpected
/// codex variants). Modern Codex CLI (>= 2026-03) emits `task_complete` per
/// turn and the read loop completes immediately when it observes that signal
/// + zero pending tool calls — this drain is only the safety net.
///
/// Issue #2423: previous value of 750ms produced premature `Done` whenever
/// Codex paused for >750ms between rollout writes (e.g. tool-call burst
/// boundary), truncating the assistant response. Issue #2419 / PR #2422 bumped
/// this to 5000ms as a heuristic; #2423 replaces the heuristic with an
/// explicit `task_complete` detection and keeps the 5s drain only for
/// legacy/unknown rollout variants. Tool-call gating (see
/// `RolloutParseState::pending_tool_calls`) is the structural complement that
/// suppresses drain entirely while one or more tools are in flight.
const DEFAULT_TERMINAL_DRAIN_MS: u64 = 5000;
/// Issue #2453: legacy Codex CLI builds (no `event_msg` records at all in the
/// rollout) cannot benefit from the `task_complete` fast-path nor the
/// token-count refresh signal. The 5s terminal drain stays structurally
/// fragile against burst-pause-burst patterns with >5s mid-response pauses.
/// When the tail observes ZERO `event_msg` records on a turn — the
/// canonical legacy-CLI fingerprint — the drain budget is bumped to this
/// longer value so a single quiet window must persist that long before the
/// tail flushes `Done`. Modern CLIs always emit at least one `event_msg`
/// (token_count is emitted alongside every message) and therefore retain the
/// shorter 5s drain. See issue #2453 / PR #2432 follow-up.
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

#[derive(Debug, Default)]
struct RolloutParseState {
    session_id: Option<String>,
    final_text: String,
    saw_assistant_text: bool,
    lines_read: usize,
    bytes_read: u64,
    /// #2419: tracks tool calls that are currently in flight (observed
    /// `function_call` / `custom_tool_call` lines whose matching `*_output`
    /// line has not arrived yet). Keyed by `call_id` so that multiple
    /// concurrent tool calls are tracked independently — a single boolean
    /// would be cleared by the first tool output even while later calls
    /// remain pending, allowing the drain timer to fire prematurely.
    ///
    /// `pending_tool_calls_unkeyed` is a fallback counter for lines that
    /// omit `call_id` (defensive — Codex normally emits one).
    ///
    /// #2423 reuse: `has_pending_tool_call()` is the canonical predicate the
    /// `task_complete` fast-path also consults, so a single source of truth
    /// guards both the drain heuristic and the explicit completion path.
    pending_tool_calls: HashSet<String>,
    pending_tool_calls_unkeyed: usize,
    /// #2419: set by `process_rollout_line` per-call when a rollout record
    /// represents lifecycle activity (assistant text, tool-call lifecycle,
    /// reasoning) even if no `StreamMessage` ends up being emitted (e.g. an
    /// empty `function_call_output`). Used to refresh the drain clock so
    /// the timer does not fire immediately after a silent tool resolution
    /// while the post-tool assistant text is still being written.
    lifecycle_activity: bool,
    /// #2423: set when an `event_msg/task_complete` rollout entry has been
    /// observed. The read loop uses this together with
    /// `has_pending_tool_call()` to short-circuit the EOF drain timer and
    /// emit `Done` immediately.
    turn_complete_seen: bool,
    /// #2423: `last_agent_message` field captured from
    /// `event_msg/task_complete`. Used as a fallback for `Done.result` when
    /// the turn produced no `response_item/message` assistant text (e.g. a
    /// tool-only turn).
    task_complete_fallback_text: Option<String>,
    /// #2453: set the first time the tail observes ANY `event_msg` rollout
    /// record (e.g. `token_count`, `agent_reasoning`, `task_complete`).
    /// Modern Codex CLI builds emit at least one such record per turn —
    /// typically `token_count` after each message. When this flag stays
    /// `false` at EOF, the tail is reading a legacy rollout writer and
    /// must apply the longer `legacy_terminal_drain` to absorb >5s mid-
    /// response pauses without truncating bursts.
    seen_any_event_msg: bool,
    /// Optional tmux session owner used to classify rollout user messages as
    /// SSH-direct input versus Discord-routed duplicates.
    tmux_session_name: Option<String>,
    /// Turn-local launch prompt used to suppress the first matching rollout
    /// user message even when the global pending TTL has elapsed.
    discord_origin_prompt: Option<String>,
}

impl RolloutParseState {
    fn record(&mut self, line_len: usize) {
        self.lines_read += 1;
        self.bytes_read += line_len as u64;
    }

    fn has_pending_tool_call(&self) -> bool {
        !self.pending_tool_calls.is_empty() || self.pending_tool_calls_unkeyed > 0
    }
}

pub fn default_codex_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .map(|home| home.join("sessions"))
}

pub fn tail_latest_rollout_for_cwd(
    cwd: &Path,
    modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
) -> Result<ReadOutputResult, String> {
    tail_latest_rollout_for_cwd_with_options(
        cwd,
        modified_since,
        sender,
        cancel_token,
        is_alive,
        RolloutTailOptions::default(),
    )
}

pub(crate) fn observe_rollout_turn_state(
    rollout_path: &Path,
) -> crate::services::tui_turn_state::TuiTurnState {
    crate::services::tui_turn_state::observe_codex_jsonl_turn_state(rollout_path)
}

pub fn tail_latest_rollout_for_cwd_with_handoff(
    cwd: &Path,
    modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
) -> Result<CodexTuiTailResult, String> {
    tail_latest_rollout_for_cwd_with_handoff_options(
        cwd,
        modified_since,
        sender,
        cancel_token,
        is_alive,
        RolloutTailOptions::default(),
    )
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

pub fn tail_latest_rollout_for_cwd_with_options(
    cwd: &Path,
    modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
    options: RolloutTailOptions,
) -> Result<ReadOutputResult, String> {
    tail_latest_rollout_for_cwd_with_handoff_options(
        cwd,
        modified_since,
        sender,
        cancel_token,
        is_alive,
        options,
    )
    .map(|result| result.read_result)
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

pub fn tail_resumed_rollout_for_session(
    cwd: &Path,
    session_id: &str,
    previous_rollout_path: &Path,
    previous_start_offset: u64,
    modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
) -> Result<ReadOutputResult, String> {
    let sessions_dir = default_codex_sessions_dir()
        .ok_or_else(|| "Codex sessions directory is unavailable".to_string())?;
    tail_resumed_rollout_for_session_with_options(
        cwd,
        session_id,
        previous_rollout_path,
        previous_start_offset,
        modified_since,
        &sessions_dir,
        sender,
        cancel_token,
        is_alive,
        RolloutTailOptions::default(),
    )
}

pub fn tail_resumed_rollout_for_session_with_handoff(
    cwd: &Path,
    session_id: &str,
    previous_rollout_path: &Path,
    previous_start_offset: u64,
    modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    is_alive: impl FnMut() -> bool,
) -> Result<CodexTuiTailResult, String> {
    let sessions_dir = default_codex_sessions_dir()
        .ok_or_else(|| "Codex sessions directory is unavailable".to_string())?;
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
        RolloutTailOptions::default(),
    )
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

pub fn latest_rollout_for_cwd_since(
    cwd: &Path,
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
        if !rollout_session_cwd_matches(&path, &canonical_cwd) {
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

fn rollout_files_under(root: &Path) -> Vec<PathBuf> {
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
                // #2423: explicit turn-complete fast-path. The Codex CLI
                // emits exactly one `event_msg/task_complete` per turn (see
                // `event_msg_message` for the contract). When we have
                // observed it AND every in-flight `function_call` has been
                // matched by its `_output` (via `has_pending_tool_call()`
                // from #2419), emit `Done` immediately — do not wait for the
                // `terminal_drain` heuristic which can truncate responses on
                // legitimate burst boundaries.
                if enable_task_complete_fast_path
                    && state.turn_complete_seen
                    && !state.has_pending_tool_call()
                {
                    // Recover the assistant text from `last_agent_message`
                    // when the turn produced no `response_item/message`
                    // (tool-only turns or rollouts where the assistant text
                    // is only carried on `task_complete`).
                    if !state.saw_assistant_text
                        && let Some(text) = state.task_complete_fallback_text.take()
                    {
                        if !state.final_text.is_empty() {
                            state.final_text.push_str("\n\n");
                        }
                        state.final_text.push_str(&text);
                        state.saw_assistant_text = true;
                    }
                    // schema-drift guard (codex review HIGH-2): if task_complete
                    // fires but `last_agent_message` was absent (field renamed or
                    // removed in a future codex CLI build), do not emit an empty
                    // Done. Fall through to drain fallback so any subsequent
                    // assistant text still gets a chance to arrive.
                    if state.saw_assistant_text {
                        emit_done(&sender, &state);
                        return Ok((
                            ReadOutputResult::Completed {
                                offset: current_offset,
                            },
                            outcome(&state, current_offset),
                        ));
                    } else {
                        tracing::warn!(
                            "codex rollout task_complete missing last_agent_message; falling back to drain"
                        );
                    }
                }
                // #2419: only consider the turn drainable when no tool call
                // is currently in flight. Otherwise the natural silence while
                // codex waits for the tool result would trip the timer.
                //
                // #2453: when the tail has yet to observe ANY `event_msg`
                // record (legacy Codex CLI fingerprint), bump the drain to
                // `legacy_terminal_drain`. Modern CLIs emit at least one
                // `event_msg` per message, so this branch only widens the
                // drain for legacy writers that lack the structural signals
                // (token_count refresh, task_complete fast-path) the 5s
                // default leans on. The bump is applied only when the
                // configured legacy drain is strictly longer than the
                // base drain — `terminal_drain.is_zero()` (replay) and
                // shorter overrides stay verbatim.
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
                        emit_done(&sender, &state);
                        return Ok((
                            ReadOutputResult::Completed {
                                offset: current_offset,
                            },
                            outcome(&state, current_offset),
                        ));
                    }
                }
                // #2419 follow-up (Codex review HIGH): bounded recovery for
                // a pending tool call whose `*_output` never arrives (hung
                // tool, malformed line, call_id skew). Without this, the
                // drain gate stays shut forever while the pane is alive and
                // the Discord turn hangs. After `pending_tool_call_deadline`
                // of inactivity past the last lifecycle event we surface a
                // terminal Done with a warning so the upstream advances.
                //
                // #2429 HIGH 2 (tool-first stuck calls bypass recovery):
                // when `apply_pending_tool_deadline_without_assistant_text`
                // is enabled (default), the deadline also fires for a tool
                // that hangs BEFORE any assistant text has been emitted —
                // previously such turns fell back to the 30 min global
                // deadline. The warning copy distinguishes the no-text case
                // so operators can spot tool-first stuck calls in logs.
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
                        emit_done(&sender, &state);
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
                // #2182 follow-up: global deadline guard. Without this, a
                // stuck/hung Codex TUI keeps the tailer alive indefinitely
                // and the upstream turn never sees `StreamMessage::Done`.
                if !state.saw_assistant_text
                    && let Some(deadline) = assistant_response_deadline
                    && started_at.elapsed() >= deadline
                {
                    let elapsed_secs = started_at.elapsed().as_secs();
                    tracing::warn!(
                        rollout_path = %rollout_path.display(),
                        elapsed_secs,
                        "Codex rollout tail timed out waiting for assistant response; emitting Done"
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

fn emit_done(sender: &RelaySuppressionSender<'_>, state: &RolloutParseState) {
    sender.send(StreamMessage::Done {
        result: state.final_text.clone(),
        session_id: state.session_id.clone(),
    });
}

fn process_rollout_line(
    line: &str,
    sender: &RelaySuppressionSender<'_>,
    state: &mut RolloutParseState,
) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(json) = serde_json::from_str::<Value>(trimmed) else {
        tracing::debug!("ignoring malformed Codex rollout line");
        return false;
    };

    // #2419: capture lifecycle activity per-line. Tool-call/tool-output
    // records count as activity even when they do not produce a
    // StreamMessage (empty output, missing name, etc.), so the drain clock
    // must refresh on them too.
    state.lifecycle_activity = false;
    let messages = rollout_messages(&json, state);
    observe_rollout_user_prompt(&json, state);
    let emitted = !messages.is_empty();
    for message in messages {
        sender.send(message);
    }
    let activity = emitted || state.lifecycle_activity;
    state.lifecycle_activity = false;
    activity
}

fn process_rollout_line_bytes(
    line: &[u8],
    sender: &RelaySuppressionSender<'_>,
    state: &mut RolloutParseState,
) -> bool {
    let Ok(line) = std::str::from_utf8(line) else {
        tracing::debug!("ignoring non-UTF-8 Codex rollout line");
        return false;
    };
    process_rollout_line(line, sender, state)
}

fn rollout_messages(json: &Value, state: &mut RolloutParseState) -> Vec<StreamMessage> {
    match json.get("type").and_then(Value::as_str).unwrap_or("") {
        "session_meta" => session_meta_message(json, state).into_iter().collect(),
        "response_item" => response_item_messages(json, state),
        "event_msg" => event_msg_message(json, state).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn observe_rollout_user_prompt(json: &Value, state: &mut RolloutParseState) {
    let Some(tmux_session_name) = state.tmux_session_name.clone() else {
        return;
    };
    let Some(prompt) = crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt(json)
    else {
        return;
    };
    if state
        .discord_origin_prompt
        .as_deref()
        .is_some_and(|expected| {
            crate::services::tui_prompt_dedupe::prompts_match(expected, &prompt)
        })
    {
        crate::services::tui_prompt_dedupe::record_suppressed_discord_origin_prompt(
            "codex",
            &tmux_session_name,
            &prompt,
        );
        state.discord_origin_prompt = None;
        tracing::debug!(
            tmux_session_name,
            "suppressed Codex launch prompt observed in rollout"
        );
        return;
    }
    let observation = crate::services::tui_prompt_dedupe::observe_prompt_by_tmux(
        "codex",
        &tmux_session_name,
        &prompt,
    );
    tracing::debug!(
        tmux_session_name,
        observation = ?observation,
        "observed Codex rollout user prompt"
    );
}

fn session_meta_message(json: &Value, state: &mut RolloutParseState) -> Option<StreamMessage> {
    let session_id = json
        .get("payload")
        .and_then(|payload| payload.get("id"))
        .and_then(Value::as_str)?
        .trim();
    if session_id.is_empty() {
        return None;
    }
    state.session_id = Some(session_id.to_string());
    Some(StreamMessage::Init {
        session_id: session_id.to_string(),
        raw_session_id: None,
    })
}

fn response_item_messages(json: &Value, state: &mut RolloutParseState) -> Vec<StreamMessage> {
    let Some(payload) = json.get("payload") else {
        return Vec::new();
    };
    match payload.get("type").and_then(Value::as_str).unwrap_or("") {
        "message" => response_message_items(payload, state),
        "function_call" | "custom_tool_call" => {
            // #2419: a tool call has started — suppress terminal_drain Done
            // until the matching output line arrives. Track by `call_id` so
            // that concurrent tool calls all hold the gate open until each
            // one resolves independently.
            //
            // #2423: this same predicate (`has_pending_tool_call`) also gates
            // the explicit `task_complete` fast-path so a rollout where the
            // writer emits `task_complete` slightly before the final tool
            // output line still drains correctly.
            match payload.get("call_id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => {
                    state.pending_tool_calls.insert(id.to_string());
                }
                _ => {
                    state.pending_tool_calls_unkeyed =
                        state.pending_tool_calls_unkeyed.saturating_add(1);
                }
            }
            // #2419: lifecycle activity — refresh drain clock even if the
            // tool_call_message ends up empty (e.g. missing name field).
            state.lifecycle_activity = true;
            tool_call_message(payload).into_iter().collect()
        }
        "function_call_output" | "custom_tool_call_output" => {
            // #2419: tool call resolved — release that specific call's hold
            // on the drain gate. Other pending calls (if any) keep it shut.
            // Saturating sub on the unkeyed counter tolerates start-mid-turn
            // tail resumes where we missed the opening `function_call` line.
            match payload.get("call_id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => {
                    state.pending_tool_calls.remove(id);
                }
                _ => {
                    state.pending_tool_calls_unkeyed =
                        state.pending_tool_calls_unkeyed.saturating_sub(1);
                }
            }
            // #2419: lifecycle activity — drain clock must be reset even
            // when the tool output is empty, otherwise EOF immediately after
            // an empty resolution can fire the drain timer before the
            // post-tool assistant text is appended.
            state.lifecycle_activity = true;
            tool_result_message(payload).into_iter().collect()
        }
        "reasoning" => {
            state.lifecycle_activity = true;
            vec![StreamMessage::redacted_thinking()]
        }
        _ => Vec::new(),
    }
}

fn response_message_items(payload: &Value, state: &mut RolloutParseState) -> Vec<StreamMessage> {
    if payload.get("role").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|item| {
            let item_type = item.get("type").and_then(Value::as_str)?;
            if item_type != "output_text" && item_type != "text" {
                return None;
            }
            let text = item.get("text").and_then(Value::as_str)?.to_string();
            if text.is_empty() {
                return None;
            }
            state.saw_assistant_text = true;
            if !state.final_text.is_empty() {
                state.final_text.push_str("\n\n");
            }
            state.final_text.push_str(&text);
            Some(StreamMessage::Text { content: text })
        })
        .collect()
}

fn tool_call_message(payload: &Value) -> Option<StreamMessage> {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())?;
    let input = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("action"))
        .map(compact_json_or_string)
        .unwrap_or_else(|| "{}".to_string());
    Some(StreamMessage::ToolUse {
        name: name.to_string(),
        input,
    })
}

fn tool_result_message(payload: &Value) -> Option<StreamMessage> {
    let content = payload
        .get("output")
        .or_else(|| payload.get("content"))
        .map(compact_json_or_string)?;
    if content.is_empty() {
        return None;
    }
    Some(StreamMessage::ToolResult {
        content,
        is_error: payload
            .get("is_error")
            .or_else(|| payload.get("isError"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn event_msg_message(json: &Value, state: &mut RolloutParseState) -> Option<StreamMessage> {
    let payload = json.get("payload")?;
    // #2453: any `event_msg` record fingerprints a modern Codex CLI. The
    // drain decision in the read loop branches on this flag — legacy
    // writers (no event_msg whatsoever) use the longer drain so a >5s
    // burst-pause-burst response does not truncate.
    state.seen_any_event_msg = true;
    match payload.get("type").and_then(Value::as_str)? {
        "token_count" => token_count_status(payload),
        "agent_reasoning" => Some(StreamMessage::redacted_thinking()),
        "task_complete" => {
            // #2423: Codex CLI (>= 2026-03) emits exactly one
            // `event_msg/task_complete` per turn, after every assistant
            // message and tool-call output, carrying `last_agent_message` and
            // `duration_ms`. This is the canonical turn-end signal — far
            // safer than the EOF drain timer (which can fire mid-burst on
            // legitimate pauses, truncating the response — see #2419/#2422).
            //
            // We do not emit a `StreamMessage` here. The terminal `Done` is
            // synthesized in the read loop after observing
            // `turn_complete_seen` + `!has_pending_tool_call()`.
            state.turn_complete_seen = true;
            if state.task_complete_fallback_text.is_none() {
                state.task_complete_fallback_text = payload
                    .get("last_agent_message")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
            }
            None
        }
        _ => {
            // #2477: Codex emits a mix of progress/lifecycle event_msg records
            // while long tools and subagents are still running. Most of them
            // do not map to a user-visible StreamMessage, but they still prove
            // the turn is alive. Refresh the EOF drain clock so the
            // saw_assistant_text fallback cannot finalize the turn in the
            // middle of a natural tool/subagent pause.
            state.lifecycle_activity = true;
            None
        }
    }
}

fn token_count_status(payload: &Value) -> Option<StreamMessage> {
    let info = payload.get("info")?;
    let usage = info
        .get("last_token_usage")
        .or_else(|| info.get("total_token_usage"))?;
    Some(StreamMessage::StatusUpdate {
        model: None,
        cost_usd: None,
        total_cost_usd: None,
        duration_ms: None,
        num_turns: None,
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        cache_create_tokens: None,
        cache_read_tokens: usage.get("cached_input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
    })
}

fn compact_json_or_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default())
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
            StreamMessage::ToolUse { name, input }
                if name == "exec_command" && input.contains("\"cmd\":\"date\"")
        ));
        assert!(matches!(
            &messages[2],
            StreamMessage::ToolResult { content, is_error }
                if content.contains("Process exited with code 0") && !is_error
        ));
        assert!(matches!(
            &messages[3],
            StreamMessage::Text { content } if content == "hello from rollout"
        ));
        assert!(matches!(
            &messages[4],
            StreamMessage::StatusUpdate {
                input_tokens: Some(7),
                cache_read_tokens: Some(3),
                output_tokens: Some(2),
                ..
            }
        ));
        assert!(matches!(
            messages.last(),
            Some(StreamMessage::Done { result, session_id })
                if result == "hello from rollout"
                    && session_id.as_deref() == Some("rollout-session")
        ));
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
            messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "segment2")),
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
            messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "segment2")),
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
            messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "segment2")),
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
            messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "segment2")),
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
            messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "segment2")),
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
    #[allow(clippy::too_many_arguments)]
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
            messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "burst2")),
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
}
