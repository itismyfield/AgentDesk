use serde_json::Value;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::{CancelToken, ReadOutputResult, cancel_requested};

const DEFAULT_ROLLOUT_WAIT_SECS: u64 = 30;
const DEFAULT_TERMINAL_DRAIN_MS: u64 = 750;
/// Upper bound on how long the tailer will sit at EOF waiting for the assistant
/// response to begin streaming. Without this guard, a stuck Codex TUI (tool
/// loop, network hang, etc.) keeps the tailer thread alive indefinitely and the
/// caller never sees a terminal `StreamMessage::Done`.
const DEFAULT_ASSISTANT_RESPONSE_DEADLINE_SECS: u64 = 30 * 60;

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
}

impl Default for RolloutTailOptions {
    fn default() -> Self {
        Self {
            wait_for_rollout: Duration::from_secs(DEFAULT_ROLLOUT_WAIT_SECS),
            terminal_drain: Duration::from_millis(DEFAULT_TERMINAL_DRAIN_MS),
            assistant_response_deadline: Some(Duration::from_secs(
                DEFAULT_ASSISTANT_RESPONSE_DEADLINE_SECS,
            )),
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
}

impl RolloutParseState {
    fn record(&mut self, line_len: usize) {
        self.lines_read += 1;
        self.bytes_read += line_len as u64;
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
        ..RolloutParseState::default()
    };
    let mut current_offset = seek_offset;
    let mut partial_line = Vec::new();
    let mut buf = [0u8; 8192];
    let mut last_output_at: Option<Instant> = None;
    let started_at = Instant::now();

    loop {
        if cancel_requested(cancel_token.as_deref()) {
            return Ok((
                ReadOutputResult::Cancelled {
                    offset: current_offset,
                },
                outcome(&state, current_offset),
            ));
        }

        match file.read(&mut buf) {
            Ok(0) => {
                if try_process_complete_partial_line(&mut partial_line, sender, &mut state) {
                    last_output_at = Some(Instant::now());
                    continue;
                }
                if state.saw_assistant_text {
                    if terminal_drain.is_zero()
                        || last_output_at.is_some_and(|at| at.elapsed() >= terminal_drain)
                    {
                        emit_done(sender, &state);
                        return Ok((
                            ReadOutputResult::Completed {
                                offset: current_offset,
                            },
                            outcome(&state, current_offset),
                        ));
                    }
                }
                if !is_alive() {
                    let result = if state.saw_assistant_text {
                        emit_done(sender, &state);
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
                    let _ = sender.send(StreamMessage::Done {
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
                while let Some(pos) = partial_line.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = partial_line.drain(..=pos).collect();
                    state.record(line.len());
                    if process_rollout_line_bytes(&line, sender, &mut state) {
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

fn try_process_complete_partial_line(
    partial_line: &mut Vec<u8>,
    sender: &Sender<StreamMessage>,
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

fn emit_done(sender: &Sender<StreamMessage>, state: &RolloutParseState) {
    let _ = sender.send(StreamMessage::Done {
        result: state.final_text.clone(),
        session_id: state.session_id.clone(),
    });
}

fn process_rollout_line(
    line: &str,
    sender: &Sender<StreamMessage>,
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

    let messages = rollout_messages(&json, state);
    let emitted = !messages.is_empty();
    for message in messages {
        let _ = sender.send(message);
    }
    emitted
}

fn process_rollout_line_bytes(
    line: &[u8],
    sender: &Sender<StreamMessage>,
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
        "event_msg" => event_msg_message(json).into_iter().collect(),
        _ => Vec::new(),
    }
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
        "function_call" | "custom_tool_call" => tool_call_message(payload).into_iter().collect(),
        "function_call_output" | "custom_tool_call_output" => {
            tool_result_message(payload).into_iter().collect()
        }
        "reasoning" => vec![StreamMessage::redacted_thinking()],
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

fn event_msg_message(json: &Value) -> Option<StreamMessage> {
    let payload = json.get("payload")?;
    match payload.get("type").and_then(Value::as_str)? {
        "token_count" => token_count_status(payload),
        "agent_reasoning" => Some(StreamMessage::redacted_thinking()),
        _ => None,
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
}
