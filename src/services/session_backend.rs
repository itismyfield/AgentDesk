//! Shared session/process backend utilities for AI provider wrappers.
//!
//! This module owns:
//! - direct child-process wrapper spawning (`ProcessBackend`)
//! - the shared in-memory process session registry
//! - normalized output-file tailing/parsing for wrapper JSONL streams

use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::{
    StreamMessage, TaskNotificationKind, status_events_from_workflow_json,
};
use crate::services::provider::{CancelToken, ReadOutputResult, SessionProbe};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOutputFailure {
    pub error: String,
    pub last_offset: u64,
}

/// Configuration for creating a new session.
pub struct SessionConfig {
    /// Unique session name (used for temp file naming)
    pub session_name: String,
    /// Working directory for the AI provider
    pub working_dir: String,
    /// Path to the agentdesk binary (for spawning wrapper)
    pub agentdesk_exe: String,
    /// Output JSONL file path
    pub output_path: String,
    /// Prompt file path
    pub prompt_path: String,
    /// Wrapper subcommand (e.g., tmux-wrapper, codex-tmux-wrapper, qwen-tmux-wrapper)
    pub wrapper_subcommand: String,
    /// Provider-specific wrapper args (e.g., --codex-bin, -- claude ...)
    pub wrapper_args: Vec<String>,
    /// Environment variables to set
    pub env_vars: Vec<(String, String)>,
}

/// Handle to a running session, returned by create_session.
pub enum SessionHandle {
    Process {
        child_stdin: Arc<Mutex<Option<ChildStdin>>>,
        child: Arc<Mutex<Option<Child>>>,
        pid: u32,
    },
}

/// Backend for managing AI provider sessions.
pub trait SessionBackend: Send + Sync {
    /// Create a new session. Returns a handle for subsequent operations.
    fn create_session(&self, config: &SessionConfig) -> Result<SessionHandle, String>;

    /// Send a follow-up message to an existing session (stream-json formatted).
    fn send_input(&self, handle: &SessionHandle, message: &str) -> Result<(), String>;

    /// Check if the session process is still running.
    fn is_alive(&self, handle: &SessionHandle) -> bool;
}

// ─── ProcessBackend ───────────────────────────────────────────────────────────

pub struct ProcessBackend;

impl ProcessBackend {
    pub fn new() -> Self {
        Self
    }
}

impl SessionBackend for ProcessBackend {
    fn create_session(&self, config: &SessionConfig) -> Result<SessionHandle, String> {
        // 1. Ensure output file exists (empty)
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.output_path)
            .map_err(|e| format!("Failed to create output file: {}", e))?;

        // 2. Build wrapper command args
        let mut args = vec![
            config.wrapper_subcommand.clone(),
            "--output-file".to_string(),
            config.output_path.clone(),
            "--input-fifo".to_string(),
            // Pipe mode doesn't use a FIFO, but the wrapper CLI still requires
            // this arg.  Use a path under the runtime temp dir so cleanup's
            // remove_file() can never hit a real user file.
            {
                #[cfg(unix)]
                {
                    crate::services::tmux_common::session_temp_path(
                        &config.session_name,
                        "unused-fifo",
                    )
                }
                #[cfg(not(unix))]
                {
                    let tmp = std::env::temp_dir()
                        .join(format!("agentdesk-{}-unused-fifo", config.session_name));
                    tmp.display().to_string()
                }
            },
            "--prompt-file".to_string(),
            config.prompt_path.clone(),
            "--cwd".to_string(),
            config.working_dir.clone(),
            "--input-mode".to_string(),
            "pipe".to_string(),
        ];
        args.extend(config.wrapper_args.clone());

        // 3. Spawn wrapper directly as child process.
        // Create a new process group so kill_pid_tree(-pid) can clean up
        // the entire subtree (wrapper + Claude/Codex child) on cancel.
        let mut cmd = Command::new(&config.agentdesk_exe);
        cmd.args(&args)
            .envs(config.env_vars.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::null()) // wrapper writes to file, not stdout
            .stderr(Stdio::inherit()); // show wrapper logs

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0); // new process group = wrapper PID
        }

        #[cfg(not(unix))]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
            // CREATE_NO_WINDOW gives the wrapper a hidden console that children
            // inherit. Without this, every cmd.exe spawned by Claude/Codex
            // creates its own *visible* console window when the parent process
            // has no console (e.g. running as a Windows service via NSSM).
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn wrapper process: {}", e))?;

        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture child stdin".to_string())?;

        Ok(SessionHandle::Process {
            child_stdin: Arc::new(Mutex::new(Some(stdin))),
            child: Arc::new(Mutex::new(Some(child))),
            pid,
        })
    }

    fn send_input(&self, handle: &SessionHandle, message: &str) -> Result<(), String> {
        match handle {
            SessionHandle::Process { child_stdin, .. } => {
                let mut guard = child_stdin
                    .lock()
                    .map_err(|e| format!("stdin lock poisoned: {}", e))?;
                if let Some(ref mut stdin) = *guard {
                    writeln!(stdin, "{}", message)
                        .map_err(|e| format!("Failed to write to child stdin: {}", e))?;
                    stdin
                        .flush()
                        .map_err(|e| format!("Failed to flush child stdin: {}", e))?;
                    Ok(())
                } else {
                    Err("Child stdin already closed".to_string())
                }
            }
        }
    }

    fn is_alive(&self, handle: &SessionHandle) -> bool {
        match handle {
            SessionHandle::Process { child, .. } => {
                let mut guard = match child.lock() {
                    Ok(g) => g,
                    Err(_) => return false,
                };
                if let Some(ref mut c) = *guard {
                    matches!(c.try_wait(), Ok(None))
                } else {
                    false
                }
            }
        }
    }
}

impl SessionHandle {
    pub fn pid(&self) -> u32 {
        match self {
            SessionHandle::Process { pid, .. } => *pid,
        }
    }
}

static PROCESS_HANDLES: LazyLock<Mutex<HashMap<String, SessionHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn process_handles() -> MutexGuard<'static, HashMap<String, SessionHandle>> {
    PROCESS_HANDLES.lock().unwrap_or_else(|error| {
        tracing::warn!("Recovered poisoned PROCESS_HANDLES mutex; continuing with inner state");
        error.into_inner()
    })
}

pub fn insert_process_session(session_name: impl Into<String>, handle: SessionHandle) {
    process_handles().insert(session_name.into(), handle);
}

pub fn remove_process_session(session_name: &str) -> Option<SessionHandle> {
    process_handles().remove(session_name)
}

pub fn process_session_pid(session_name: &str) -> Option<u32> {
    process_handles().get(session_name).map(SessionHandle::pid)
}

pub fn process_session_is_alive(session_name: &str) -> bool {
    let handles = process_handles();
    handles
        .get(session_name)
        .map(|handle| ProcessBackend::new().is_alive(handle))
        .unwrap_or(false)
}

pub fn send_process_session_input(session_name: &str, message: &str) -> Result<(), String> {
    let handles = process_handles();
    let handle = handles
        .get(session_name)
        .ok_or_else(|| format!("No process handle found for session {}", session_name))?;
    ProcessBackend::new().send_input(handle, message)
}

pub fn process_session_probe(session_name: &str) -> SessionProbe {
    let session_name = session_name.to_string();
    SessionProbe::process(move || process_session_is_alive(&session_name))
}

pub fn terminate_process_handle(handle: SessionHandle) {
    let SessionHandle::Process { child, .. } = handle;
    let mut child_guard = match child.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if let Some(ref mut process) = *child_guard {
        let _ = process.kill();
        let _ = process.wait();
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamLineState {
    pub last_session_id: Option<String>,
    pub last_model: Option<String>,
    /// #1918: input/cache_read/cache_create record the **last** API call's
    /// per-message usage so the status panel Context line reflects current
    /// context occupancy (sum across multi-call turns inflated past the
    /// window). `accum_output_tokens` stays cumulative because turn analytics
    /// and persisted token totals expect the sum across all calls.
    pub accum_input_tokens: u64,
    pub accum_cache_create_tokens: u64,
    pub accum_cache_read_tokens: u64,
    pub accum_output_tokens: u64,
    /// True once any per-message `usage` block has been observed in the
    /// stream. Lets the result-event handler fall back to `result.usage`
    /// only for providers (e.g. Qwen) that emit token counts solely on the
    /// terminal result event.
    pub saw_per_message_usage: bool,
    pub final_result: Option<String>,
    pub stdout_error: Option<(String, String)>,
    pub tool_use_names: HashMap<String, String>,
    pub task_starts: HashMap<String, TaskStartInfo>,
}

impl StreamLineState {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskStartInfo {
    pub tool_use_id: Option<String>,
    pub task_type: Option<String>,
}

pub fn extract_turn_analytics_from_output(
    output_path: &str,
    start_offset: u64,
) -> (Option<String>, Option<TurnTokenUsage>) {
    extract_turn_analytics_from_output_range(output_path, start_offset, None)
}

pub fn extract_turn_analytics_from_output_range(
    output_path: &str,
    start_offset: u64,
    end_offset: Option<u64>,
) -> (Option<String>, Option<TurnTokenUsage>) {
    let Ok(bytes) = std::fs::read(output_path) else {
        return (None, None);
    };
    let end = end_offset
        .and_then(|offset| usize::try_from(offset).ok())
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(end))
        .unwrap_or(end);

    let (sender, _receiver) = std::sync::mpsc::channel::<StreamMessage>();
    let mut state = StreamLineState::new();
    for line in String::from_utf8_lossy(&bytes[start..end]).lines() {
        let _ = process_stream_line(line, &sender, &mut state);
    }

    let usage = TurnTokenUsage {
        input_tokens: state.accum_input_tokens,
        cache_create_tokens: state.accum_cache_create_tokens,
        cache_read_tokens: state.accum_cache_read_tokens,
        output_tokens: state.accum_output_tokens,
    };
    let has_usage = usage.input_tokens > 0
        || usage.cache_create_tokens > 0
        || usage.cache_read_tokens > 0
        || usage.output_tokens > 0;

    (state.last_session_id, has_usage.then_some(usage))
}

/// Process a single normalized wrapper JSONL line.
///
/// Unknown or malformed Claude envelope types are non-terminal: they are
/// ignored and return `true` so future TUI history metadata cannot end the
/// turn reader early. `false` is reserved for a disconnected sender channel.
pub fn process_stream_line(
    line: &str,
    sender: &Sender<StreamMessage>,
    state: &mut StreamLineState,
) -> bool {
    if line.trim().is_empty() {
        return true;
    }

    let json = match serde_json::from_str::<Value>(line) {
        Ok(json) => json,
        Err(_) => return true,
    };

    let msg_type = json
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");

    if msg_type == "assistant" {
        if let Some(message) = json.get("message") {
            if let Some(model) = message.get("model").and_then(|value| value.as_str()) {
                state.last_model = Some(model.to_string());
            }
            if let Some(usage) = message.get("usage") {
                // #1918: input/cache_read/cache_create replace so persisted
                // analytics reflect the LAST API call's prompt; the previous
                // sum across multi-call (tool-use loop) turns inflated the
                // recorded context tokens past the window. output_tokens stays
                // accumulated for the cumulative output metric analytics
                // expect.
                state.saw_per_message_usage = true;
                let input_tokens = usage
                    .get("input_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                let cache_read = usage
                    .get("cache_read_input_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                let cache_creation = usage
                    .get("cache_creation_input_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                state.accum_input_tokens = input_tokens;
                state.accum_cache_read_tokens = cache_read;
                state.accum_cache_create_tokens = cache_creation;
                if let Some(output_tokens) =
                    usage.get("output_tokens").and_then(|value| value.as_u64())
                {
                    state.accum_output_tokens =
                        state.accum_output_tokens.saturating_add(output_tokens);
                }
            }
        }
    }

    if msg_type == "result" {
        // #1918: Claude CLI's result.usage in multi-call turns is
        // turn-cumulative, so overwriting input/cache here would re-introduce
        // the context-token inflation the per-message branch above already
        // resolved. Only adopt result.usage when no per-message usage was
        // observed (Qwen tmux wrappers report token counts solely on the
        // terminal result event).
        if !state.saw_per_message_usage
            && let Some(usage) = json.get("usage")
        {
            let input_tokens = usage
                .get("input_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let cache_read = usage
                .get("cache_read_input_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let cache_creation = usage
                .get("cache_creation_input_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let output_tokens = usage
                .get("output_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            state.accum_input_tokens = input_tokens;
            state.accum_cache_read_tokens = cache_read;
            state.accum_cache_create_tokens = cache_creation;
            state.accum_output_tokens = output_tokens;
        }

        let cost_usd = json.get("cost_usd").and_then(|value| value.as_f64());
        let total_cost_usd = json.get("total_cost_usd").and_then(|value| value.as_f64());
        let duration_ms = json.get("duration_ms").and_then(|value| value.as_u64());
        let num_turns = json
            .get("num_turns")
            .and_then(|value| value.as_u64())
            .map(|value| value as u32);
        if cost_usd.is_some() || total_cost_usd.is_some() || state.last_model.is_some() {
            let _ = sender.send(StreamMessage::StatusUpdate {
                model: state.last_model.clone(),
                cost_usd,
                total_cost_usd,
                duration_ms,
                num_turns,
                input_tokens: (state.accum_input_tokens > 0).then_some(state.accum_input_tokens),
                cache_create_tokens: (state.accum_cache_create_tokens > 0)
                    .then_some(state.accum_cache_create_tokens),
                cache_read_tokens: (state.accum_cache_read_tokens > 0)
                    .then_some(state.accum_cache_read_tokens),
                output_tokens: (state.accum_output_tokens > 0).then_some(state.accum_output_tokens),
            });
        }
    }

    observe_stream_context(&json, state);
    if !emit_status_events_from_stream_json(&json, sender) {
        return false;
    }

    let Some(message) = parse_stream_message_with_state(&json, state) else {
        return true;
    };

    match &message {
        StreamMessage::Init { session_id, .. } => {
            state.last_session_id = Some(session_id.clone());
        }
        StreamMessage::Done { result, session_id } => {
            state.final_result = Some(result.clone());
            if session_id.is_some() {
                state.last_session_id = session_id.clone();
            }
        }
        StreamMessage::Error { message, .. } => {
            state.stdout_error = Some((message.clone(), line.to_string()));
            return true;
        }
        _ => {}
    }

    if sender.send(message).is_err() {
        return false;
    }

    for extra in parse_assistant_extra_tool_uses(&json) {
        if sender.send(extra).is_err() {
            return false;
        }
    }

    true
}

pub(crate) fn emit_status_events_from_stream_json(
    json: &Value,
    sender: &Sender<StreamMessage>,
) -> bool {
    let events = status_events_from_workflow_json(json);
    if events.is_empty() {
        return true;
    }
    sender.send(StreamMessage::StatusEvents { events }).is_ok()
}

pub fn parse_stream_message(json: &Value) -> Option<StreamMessage> {
    let mut state = StreamLineState::new();
    observe_stream_context(json, &mut state);
    parse_stream_message_with_state(json, &state)
}

pub(crate) fn parse_stream_message_with_state(
    json: &Value,
    state: &StreamLineState,
) -> Option<StreamMessage> {
    let msg_type = json.get("type")?.as_str()?;

    match msg_type {
        "system" => {
            let subtype = json.get("subtype").and_then(|value| value.as_str())?;
            match subtype {
                "init" => {
                    let session_id = json.get("session_id")?.as_str()?.to_string();
                    Some(StreamMessage::Init {
                        session_id,
                        raw_session_id: None,
                    })
                }
                "task_notification" => Some(StreamMessage::TaskNotification {
                    task_id: json
                        .get("task_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                    status: json
                        .get("status")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                    summary: json
                        .get("summary")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                    kind: classify_task_notification_kind(json, state),
                }),
                "stop_hook_summary" => Some(StreamMessage::Done {
                    result: String::new(),
                    session_id: claude_session_id(json),
                }),
                "turn_duration" => Some(StreamMessage::StatusUpdate {
                    model: state.last_model.clone(),
                    cost_usd: None,
                    total_cost_usd: None,
                    duration_ms: json
                        .get("durationMs")
                        .or_else(|| json.get("duration_ms"))
                        .and_then(|value| value.as_u64()),
                    num_turns: json
                        .get("messageCount")
                        .or_else(|| json.get("num_turns"))
                        .and_then(|value| value.as_u64())
                        .map(|value| value as u32),
                    input_tokens: (state.accum_input_tokens > 0)
                        .then_some(state.accum_input_tokens),
                    cache_create_tokens: (state.accum_cache_create_tokens > 0)
                        .then_some(state.accum_cache_create_tokens),
                    cache_read_tokens: (state.accum_cache_read_tokens > 0)
                        .then_some(state.accum_cache_read_tokens),
                    output_tokens: (state.accum_output_tokens > 0)
                        .then_some(state.accum_output_tokens),
                }),
                _ => None,
            }
        }
        "assistant" => {
            let content = json.get("message")?.get("content")?.as_array()?;
            let mut has_thinking = false;

            for item in content {
                let item_type = match item.get("type").and_then(|value| value.as_str()) {
                    Some(item_type) => item_type,
                    None => continue,
                };
                match item_type {
                    "text" => {
                        let text = item
                            .get("text")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        if !text.is_empty() {
                            return Some(StreamMessage::Text {
                                content: text.to_string(),
                            });
                        }
                    }
                    "tool_use" => {
                        let name = item
                            .get("name")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        if !name.is_empty() {
                            let input = item
                                .get("input")
                                .map(|value| {
                                    serde_json::to_string_pretty(value).unwrap_or_default()
                                })
                                .unwrap_or_default();
                            let tool_use_id = item
                                .get("id")
                                .and_then(|value| value.as_str())
                                .map(str::to_string);
                            return Some(StreamMessage::ToolUse {
                                name: name.to_string(),
                                input,
                                tool_use_id,
                            });
                        }
                    }
                    "thinking" => {
                        has_thinking = true;
                    }
                    _ => {}
                }
            }

            if has_thinking {
                return Some(StreamMessage::redacted_thinking());
            }
            None
        }
        "user" => {
            let content = json.get("message")?.get("content")?.as_array()?;
            for item in content {
                let item_type = item.get("type")?.as_str()?;
                if item_type == "tool_result" {
                    let content_text = if let Some(text) =
                        item.get("content").and_then(|value| value.as_str())
                    {
                        text.to_string()
                    } else if let Some(items) =
                        item.get("content").and_then(|value| value.as_array())
                    {
                        items
                            .iter()
                            .filter_map(|value| value.get("text").and_then(|text| text.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        String::new()
                    };
                    let is_error = item
                        .get("is_error")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let tool_use_id = item
                        .get("tool_use_id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    return Some(StreamMessage::ToolResult {
                        content: content_text,
                        is_error,
                        tool_use_id,
                    });
                }
            }
            None
        }
        "result" => {
            let is_error = json
                .get("is_error")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if is_error {
                let error_message = json
                    .get("errors")
                    .and_then(|value| value.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|value| value.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .or_else(|| {
                        json.get("result")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "Unknown error".to_string());
                return Some(StreamMessage::Error {
                    message: error_message,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            Some(StreamMessage::Done {
                result: json
                    .get("result")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string(),
                session_id: claude_session_id(json),
            })
        }
        _ => None,
    }
}

fn claude_session_id(json: &Value) -> Option<String> {
    json.get("session_id")
        .or_else(|| json.get("sessionId"))
        .and_then(|value| value.as_str())
        .map(String::from)
}

pub(crate) fn observe_stream_context(json: &Value, state: &mut StreamLineState) {
    let Some(msg_type) = json.get("type").and_then(|value| value.as_str()) else {
        return;
    };

    match msg_type {
        "assistant" => {
            let Some(content) = json
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(|content| content.as_array())
            else {
                return;
            };

            for item in content {
                if item.get("type").and_then(|value| value.as_str()) != Some("tool_use") {
                    continue;
                }
                let Some(tool_use_id) = item.get("id").and_then(|value| value.as_str()) else {
                    continue;
                };
                let Some(tool_name) = item.get("name").and_then(|value| value.as_str()) else {
                    continue;
                };
                state
                    .tool_use_names
                    .insert(tool_use_id.to_string(), tool_name.to_string());
            }
        }
        "system" => {
            if json.get("subtype").and_then(|value| value.as_str()) != Some("task_started") {
                return;
            }
            let Some(task_id) = json.get("task_id").and_then(|value| value.as_str()) else {
                return;
            };
            state.task_starts.insert(
                task_id.to_string(),
                TaskStartInfo {
                    tool_use_id: json
                        .get("tool_use_id")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string()),
                    task_type: json
                        .get("task_type")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string()),
                },
            );
        }
        _ => {}
    }
}

pub(crate) fn classify_task_notification_kind(
    json: &Value,
    state: &StreamLineState,
) -> TaskNotificationKind {
    if let Some(kind) = json
        .get("task_notification_kind")
        .and_then(|value| value.as_str())
        .and_then(TaskNotificationKind::from_str)
    {
        return kind;
    }

    let task_id = json.get("task_id").and_then(|value| value.as_str());
    let task_info = task_id.and_then(|id| state.task_starts.get(id));
    let tool_use_id = json
        .get("tool_use_id")
        .and_then(|value| value.as_str())
        .or_else(|| task_info.and_then(|info| info.tool_use_id.as_deref()));
    let tool_name = tool_use_id.and_then(|id| state.tool_use_names.get(id).map(String::as_str));
    let task_type = task_info.and_then(|info| info.task_type.as_deref());
    let summary = json
        .get("summary")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or("");

    if tool_name == Some("Monitor")
        || task_type == Some("monitor")
        || summary.starts_with("Monitor event:")
    {
        return TaskNotificationKind::MonitorAutoTurn;
    }

    if task_type == Some("local_agent") {
        return TaskNotificationKind::Subagent;
    }

    TaskNotificationKind::Background
}

/// Extract tool_use blocks that appear after an initial text block in a single
/// assistant event so downstream relay logic sees both narration and tools.
pub fn parse_assistant_extra_tool_uses(json: &Value) -> Vec<StreamMessage> {
    if json.get("type").and_then(|value| value.as_str()) != Some("assistant") {
        return Vec::new();
    }

    let content = match json
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())
    {
        Some(content) => content,
        None => return Vec::new(),
    };

    let mut saw_text_first = false;
    let mut extras = Vec::new();
    for item in content {
        let item_type = match item.get("type").and_then(|value| value.as_str()) {
            Some(item_type) => item_type,
            None => continue,
        };
        match item_type {
            "text" if extras.is_empty() => {
                let text = item
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if !text.is_empty() {
                    saw_text_first = true;
                }
            }
            "tool_use" if saw_text_first => {
                let name = item
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if !name.is_empty() {
                    let input = item
                        .get("input")
                        .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
                        .unwrap_or_default();
                    let tool_use_id = item
                        .get("id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    extras.push(StreamMessage::ToolUse {
                        name: name.to_string(),
                        input,
                        tool_use_id,
                    });
                }
            }
            _ => {}
        }
    }

    extras
}

pub fn read_output_file_until_result(
    output_path: &str,
    start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    probe: SessionProbe,
) -> Result<ReadOutputResult, String> {
    read_output_file_until_result_tracked(output_path, start_offset, sender, cancel_token, probe)
        .map_err(|failure| failure.error)
}

pub fn read_output_file_until_result_tracked(
    output_path: &str,
    start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    probe: SessionProbe,
) -> Result<ReadOutputResult, ReadOutputFailure> {
    let mut state = StreamLineState::new();
    let SessionProbe {
        is_alive,
        is_ready_for_input,
    } = probe;
    let last_offset = Arc::new(AtomicU64::new(start_offset));
    let offset_sender = sender.clone();
    let line_sender = sender.clone();
    let synthetic_sender = sender.clone();
    let error_sender = sender.clone();
    let last_offset_for_emit = last_offset.clone();

    let result = crate::services::provider::poll_output_file_until_result(
        output_path,
        start_offset,
        cancel_token,
        &mut state,
        move || is_alive(),
        move || is_ready_for_input(),
        move |offset| {
            last_offset_for_emit.store(offset, Ordering::Relaxed);
            let _ = offset_sender.send(StreamMessage::OutputOffset { offset });
        },
        move |line, state| process_stream_line(line, &line_sender, state),
        |state| state.final_result.is_some(),
        move |state| {
            synthetic_sender
                .send(StreamMessage::Done {
                    result: String::new(),
                    session_id: state.last_session_id.clone(),
                })
                .is_ok()
        },
        move |state| {
            if let Some((message, stdout_raw)) = &state.stdout_error {
                let _ = error_sender.send(StreamMessage::Error {
                    message: message.clone(),
                    stdout: stdout_raw.clone(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
        },
    );

    match result {
        Ok(result) => Ok(result),
        Err(error) => Err(ReadOutputFailure {
            error,
            last_offset: last_offset.load(Ordering::Relaxed),
        }),
    }
}

#[cfg(test)]
mod stream_tail_guard_tests {
    use super::*;
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::provider::{ReadOutputResult, SessionProbe};
    use std::io::Write;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn read_output_file_until_result_buffers_split_jsonl_line() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("stream.jsonl");
        let assistant_line =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello world"}]}}"#;
        let split_at = assistant_line.find("lo world").unwrap();
        let first_chunk = &assistant_line[..split_at];
        let second_chunk = format!(
            "{}\n{}\n",
            &assistant_line[split_at..],
            r#"{"type":"result","subtype":"success","result":"done","session_id":"sess-1"}"#
        );
        std::fs::write(&output_path, first_chunk).unwrap();

        let (sender, receiver) = mpsc::channel();
        let reader_path = output_path.to_string_lossy().into_owned();
        let reader = thread::spawn(move || {
            read_output_file_until_result(
                &reader_path,
                0,
                sender,
                None,
                SessionProbe::process(|| true),
            )
        });

        thread::sleep(Duration::from_millis(50));
        assert!(receiver.try_recv().is_err());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&output_path)
            .unwrap()
            .write_all(second_chunk.as_bytes())
            .unwrap();

        let result = reader.join().unwrap().unwrap();
        assert_eq!(
            result,
            ReadOutputResult::Completed {
                offset: (first_chunk.len() + second_chunk.len()) as u64
            }
        );
        let messages = receiver.try_iter().collect::<Vec<_>>();
        assert!(messages.iter().any(
            |message| matches!(message, StreamMessage::Text { content } if content == "hello world")
        ));
        assert!(messages.iter().any(
            |message| matches!(message, StreamMessage::Done { result, .. } if result == "done")
        ));
    }

    #[test]
    fn process_stream_line_emits_workflow_status_events() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut state = StreamLineState::new();

        assert!(process_stream_line(
            r#"{"type":"system","subtype":"task_progress","task_id":"wf-1","workflow_progress":[{"type":"workflow_phase","index":1,"title":"P1"},{"type":"workflow_agent","index":1,"label":"pinger","phaseIndex":1,"phaseTitle":"P1","state":"progress"}]}"#,
            &sender,
            &mut state,
        ));

        let message = receiver
            .try_iter()
            .find(|message| matches!(message, StreamMessage::StatusEvents { .. }))
            .expect("workflow status events");
        let StreamMessage::StatusEvents { events } = message else {
            panic!("expected StatusEvents");
        };
        assert_eq!(
            events,
            vec![
                crate::services::agent_protocol::StatusEvent::WorkflowPhase {
                    task_id: Some("wf-1".to_string()),
                    index: 1,
                    title: "P1".to_string()
                },
                crate::services::agent_protocol::StatusEvent::WorkflowAgent {
                    task_id: Some("wf-1".to_string()),
                    index: 1,
                    label: "pinger".to_string(),
                    phase_index: Some(1),
                    phase_title: Some("P1".to_string()),
                    state: "progress".to_string()
                }
            ]
        );
    }
}
