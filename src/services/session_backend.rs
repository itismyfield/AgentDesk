//! Shared session/process backend utilities for AI provider wrappers.
//!
//! This module owns:
//! - direct child-process wrapper spawning (`ProcessBackend`)
//! - the shared in-memory process session registry
//! - normalized output-file tailing/parsing for wrapper JSONL streams

use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::{CancelToken, ReadOutputResult, SessionProbe};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, LazyLock, Mutex};

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

pub fn insert_process_session(session_name: impl Into<String>, handle: SessionHandle) {
    PROCESS_HANDLES
        .lock()
        .unwrap()
        .insert(session_name.into(), handle);
}

pub fn remove_process_session(session_name: &str) -> Option<SessionHandle> {
    PROCESS_HANDLES.lock().unwrap().remove(session_name)
}

pub fn process_session_pid(session_name: &str) -> Option<u32> {
    PROCESS_HANDLES
        .lock()
        .unwrap()
        .get(session_name)
        .map(SessionHandle::pid)
}

pub fn process_session_is_alive(session_name: &str) -> bool {
    let handles = PROCESS_HANDLES.lock().unwrap();
    handles
        .get(session_name)
        .map(|handle| ProcessBackend::new().is_alive(handle))
        .unwrap_or(false)
}

pub fn send_process_session_input(session_name: &str, message: &str) -> Result<(), String> {
    let handles = PROCESS_HANDLES.lock().unwrap();
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
    pub accum_input_tokens: u64,
    pub accum_cache_create_tokens: u64,
    pub accum_cache_read_tokens: u64,
    pub accum_output_tokens: u64,
    pub final_result: Option<String>,
    pub stdout_error: Option<(String, String)>,
}

impl StreamLineState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Process a single normalized wrapper JSONL line.
/// Returns false when the sender channel is disconnected.
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
                state.accum_input_tokens += input_tokens;
                state.accum_cache_read_tokens += cache_read;
                state.accum_cache_create_tokens += cache_creation;
                if let Some(output_tokens) =
                    usage.get("output_tokens").and_then(|value| value.as_u64())
                {
                    state.accum_output_tokens += output_tokens;
                }
            }
        }
    }

    if msg_type == "result" {
        if let Some(usage) = json.get("usage") {
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

    if let Some(message) = parse_stream_message(&json) {
        match &message {
            StreamMessage::Init { session_id } => {
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
    }

    true
}

pub fn parse_stream_message(json: &Value) -> Option<StreamMessage> {
    let msg_type = json.get("type")?.as_str()?;

    match msg_type {
        "system" => {
            let subtype = json.get("subtype").and_then(|value| value.as_str())?;
            match subtype {
                "init" => {
                    let session_id = json.get("session_id")?.as_str()?.to_string();
                    Some(StreamMessage::Init { session_id })
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
                }),
                _ => None,
            }
        }
        "assistant" => {
            let content = json.get("message")?.get("content")?.as_array()?;
            let mut has_thinking = false;
            let mut thinking_summary: Option<String> = None;

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
                            return Some(StreamMessage::ToolUse {
                                name: name.to_string(),
                                input,
                            });
                        }
                    }
                    "thinking" => {
                        has_thinking = true;
                        thinking_summary = item
                            .get("thinking")
                            .and_then(|value| value.as_str())
                            .map(|value| value.trim().to_string())
                            .filter(|value| !value.is_empty());
                    }
                    _ => {}
                }
            }

            if has_thinking {
                return Some(StreamMessage::Thinking {
                    summary: thinking_summary,
                });
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
                    return Some(StreamMessage::ToolResult {
                        content: content_text,
                        is_error,
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
                session_id: json
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .map(String::from),
            })
        }
        _ => None,
    }
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
                    extras.push(StreamMessage::ToolUse {
                        name: name.to_string(),
                        input,
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
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn unique_session_name(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_nanos();
        format!("{prefix}-{nanos}")
    }

    fn spawn_stdin_sink_handle() -> SessionHandle {
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "cat >/dev/null"]);
            command
        };

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", "more > NUL"]);
            command
        };

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn stdin sink child");
        let pid = child.id();
        let stdin = child.stdin.take().expect("capture child stdin");
        SessionHandle::Process {
            child_stdin: Arc::new(Mutex::new(Some(stdin))),
            child: Arc::new(Mutex::new(Some(child))),
            pid,
        }
    }

    fn spawn_exiting_handle() -> SessionHandle {
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "exit 0"]);
            command
        };

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", "exit 0"]);
            command
        };

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn exiting child");
        let pid = child.id();
        let stdin = child.stdin.take().expect("capture child stdin");
        SessionHandle::Process {
            child_stdin: Arc::new(Mutex::new(Some(stdin))),
            child: Arc::new(Mutex::new(Some(child))),
            pid,
        }
    }

    #[test]
    fn test_process_session_registry_reuse_and_remove_roundtrip() {
        let session_name = unique_session_name("process-registry");
        insert_process_session(&session_name, spawn_stdin_sink_handle());

        assert!(process_session_is_alive(&session_name));
        assert!(process_session_pid(&session_name).is_some());
        assert!(send_process_session_input(&session_name, r#"{"type":"user"}"#).is_ok());

        let handle = remove_process_session(&session_name).expect("session handle removed");
        assert!(!process_session_is_alive(&session_name));
        terminate_process_handle(handle);
    }

    #[test]
    fn test_read_output_file_until_result_completes_with_shared_normalized_parser() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("stream.jsonl");
        std::fs::write(
            &output_path,
            concat!(
                r#"{"type":"system","subtype":"init","session_id":"sess-1"}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#,
                "\n",
                r#"{"type":"result","subtype":"success","result":"done","session_id":"sess-1"}"#,
                "\n"
            ),
        )
        .unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        let result = read_output_file_until_result(
            output_path.to_str().unwrap(),
            0,
            sender,
            None,
            SessionProbe::process(|| true),
        )
        .unwrap();

        assert_eq!(
            result,
            ReadOutputResult::Completed {
                offset: std::fs::metadata(&output_path).unwrap().len(),
            }
        );
        assert!(matches!(
            receiver.recv().unwrap(),
            StreamMessage::OutputOffset { .. }
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            StreamMessage::Init { session_id } if session_id == "sess-1"
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            StreamMessage::Text { content } if content == "hello"
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            StreamMessage::Done { result, session_id } if result == "done" && session_id.as_deref() == Some("sess-1")
        ));
    }

    #[test]
    fn test_read_output_file_until_result_reports_session_died_via_registry_probe() {
        let session_name = unique_session_name("process-dead");
        insert_process_session(&session_name, spawn_exiting_handle());
        std::thread::sleep(Duration::from_millis(50));

        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("stream.jsonl");
        std::fs::write(
            &output_path,
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#,
                "\n"
            ),
        )
        .unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        let result = read_output_file_until_result(
            output_path.to_str().unwrap(),
            0,
            sender,
            None,
            process_session_probe(&session_name),
        )
        .unwrap();

        assert_eq!(
            result,
            ReadOutputResult::SessionDied {
                offset: std::fs::metadata(&output_path).unwrap().len(),
            }
        );
        assert!(matches!(
            receiver.recv().unwrap(),
            StreamMessage::OutputOffset { .. }
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            StreamMessage::Text { content } if content == "partial"
        ));

        let handle = remove_process_session(&session_name).expect("dead process handle removed");
        terminate_process_handle(handle);
    }
}
