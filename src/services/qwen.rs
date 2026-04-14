use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;
use uuid::Uuid;

use crate::services::agent_protocol::StreamMessage;
use crate::services::claude;
use crate::services::discord::restart_report::{
    RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
};
use crate::services::process::{kill_child_tree, shell_escape};
use crate::services::provider::{
    CancelToken, FollowupResult, ProviderKind, ReadOutputResult, SessionProbe, cancel_requested,
    register_child_pid, tmux_followup_fallback_after_read_error,
};
use crate::services::provider_runtime::{
    LineStreamEvent, SharedAllowedToolKind, resolve_shared_allowed_tool_compat,
    spawn_line_stream_reader,
};
use crate::services::remote::RemoteProfile;
use crate::services::session_backend::{
    insert_process_session, process_session_is_alive, process_session_probe,
    read_output_file_until_result, read_output_file_until_result_tracked, remove_process_session,
    send_process_session_input,
};
#[cfg(unix)]
use crate::services::tmux_common::{tmux_owner_path, write_tmux_owner_marker};
#[cfg(unix)]
use crate::services::tmux_diagnostics::{
    record_tmux_exit_reason, should_recreate_session_after_followup_fifo_error,
    tmux_session_exists, tmux_session_has_live_pane,
};

const QWEN_CANCELLED_MESSAGE: &str = "Qwen request cancelled";
const QWEN_SESSION_DEAD_MESSAGE: &str = "Qwen stream ended without a terminal result";
pub(crate) const QWEN_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const QWEN_STREAM_IDLE_TICKS_BEFORE_RETRY: u32 = 2;
pub(crate) const QWEN_MAX_SESSION_RETRIES: usize = 1;
const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";
pub(crate) const QWEN_CODE_SYSTEM_SETTINGS_ENV: &str = "QWEN_CODE_SYSTEM_SETTINGS_PATH";
const QWEN_SUPPORTED_ALLOWED_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "Task",
    "TaskOutput",
    "TaskStop",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Skill",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
];

#[derive(Debug)]
pub(crate) struct QwenSystemSettingsOverride {
    path: PathBuf,
}

impl QwenSystemSettingsOverride {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for QwenSystemSettingsOverride {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

type QwenStreamEvent = LineStreamEvent;

#[derive(Clone, Debug)]
pub(crate) enum QwenResumeStrategy {
    Fresh,
    Continue,
    Resume(String),
}

#[derive(Debug)]
enum QwenAttemptResult {
    Completed,
    RetrySession {
        message: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
enum QwenStreamLoopResult {
    Eof,
    RetrySession { message: String },
    Cancelled,
}

#[derive(Debug)]
enum QwenFinalState {
    Done {
        result: String,
        session_id: Option<String>,
    },
    Error {
        message: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    RetrySession {
        message: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Default)]
struct QwenStatusSnapshot {
    model: Option<String>,
    duration_ms: Option<u64>,
    num_turns: Option<u32>,
    input_tokens: Option<u64>,
    cache_create_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Debug, Default)]
struct QwenPartialBlockState {
    kind: String,
    tool_name: Option<String>,
    input_json: String,
    thinking_signature: Option<String>,
    thinking_emitted: bool,
}

#[derive(Debug, Default)]
struct QwenAttemptState {
    final_text: String,
    raw_stdout: String,
    last_session_id: Option<String>,
    current_model: Option<String>,
    last_error_message: Option<String>,
    terminal_result_seen: bool,
    terminal_result_text: Option<String>,
    partial_stream_seen: bool,
    buffered_messages: Vec<StreamMessage>,
    partial_blocks: HashMap<usize, QwenPartialBlockState>,
    status: QwenStatusSnapshot,
}

#[allow(dead_code)]
pub fn resolve_qwen_path() -> Option<String> {
    crate::services::platform::resolve_provider_binary("qwen").resolved_path
}

fn resolve_qwen_binary() -> crate::services::platform::BinaryResolution {
    crate::services::platform::resolve_provider_binary("qwen")
}

pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    execute_command_simple_cancellable(prompt, None)
}

pub fn execute_command_simple_cancellable(
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<String, String> {
    let resolution = resolve_qwen_binary();
    let qwen_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Qwen CLI not found".to_string())?;
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let mut command = Command::new(&qwen_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    let mut child = command
        .args(build_simple_exec_args(prompt))
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Qwen: {}", e))?;

    register_child_pid(cancel_token, child.id());
    if cancel_requested(cancel_token) {
        kill_child_tree(&mut child);
        return Err("Qwen request cancelled".to_string());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to read Qwen output: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        return Err(derive_error_message(
            &stdout,
            &stderr,
            output.status.code(),
            "Qwen",
        ));
    }

    let text = extract_text_from_json_output(&stdout);
    if text.trim().is_empty() {
        Err("Empty response from Qwen".to_string())
    } else {
        Ok(text)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<Arc<CancelToken>>,
    remote_profile: Option<&RemoteProfile>,
    tmux_session_name: Option<&str>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    model: Option<&str>,
    _compact_percent: Option<u64>,
) -> Result<(), String> {
    if remote_profile.is_some() {
        return Err(remote_profile_not_supported_message());
    }

    let allowed_core_tools = resolve_allowed_core_tools(allowed_tools)?;
    let prompt = compose_qwen_prompt(prompt, system_prompt, allowed_tools);
    let resolution = resolve_qwen_binary();
    let qwen_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Qwen CLI not found".to_string())?;

    if let Some(session_name) = tmux_session_name {
        #[cfg(unix)]
        if claude::is_tmux_available() {
            return execute_streaming_local_tmux(
                &prompt,
                model,
                session_id,
                working_dir,
                sender,
                cancel_token,
                session_name,
                &resolution,
                allowed_core_tools.as_deref(),
                report_channel_id,
                report_provider,
            );
        }
        return execute_streaming_local_process(
            &prompt,
            model,
            session_id,
            working_dir,
            sender,
            cancel_token,
            session_name,
            &resolution,
            allowed_core_tools.as_deref(),
        );
    }

    let mut resume_strategy = normalize_resume_strategy(session_id, working_dir)?;

    for attempt in 0..=QWEN_MAX_SESSION_RETRIES {
        match execute_qwen_streaming_attempt(
            &qwen_bin,
            &resolution,
            &prompt,
            model,
            resume_strategy.clone(),
            working_dir,
            sender.clone(),
            cancel_token.clone(),
            allowed_core_tools.as_deref(),
            report_channel_id,
            report_provider.clone(),
        )? {
            QwenAttemptResult::Completed | QwenAttemptResult::Cancelled => return Ok(()),
            QwenAttemptResult::RetrySession {
                message,
                stdout,
                stderr,
                exit_code,
            } => {
                if attempt < QWEN_MAX_SESSION_RETRIES {
                    resume_strategy = QwenResumeStrategy::Fresh;
                    continue;
                }
                let _ = sender.send(StreamMessage::Error {
                    message: format!(
                        "Qwen session could not be recovered after retry: {}",
                        message
                    ),
                    stdout,
                    stderr,
                    exit_code,
                });
                return Ok(());
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_qwen_streaming_attempt(
    qwen_bin: &str,
    resolution: &crate::services::platform::BinaryResolution,
    prompt: &str,
    model: Option<&str>,
    resume_strategy: QwenResumeStrategy,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    allowed_core_tools: Option<&[String]>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
) -> Result<QwenAttemptResult, String> {
    let settings_override = create_system_settings_override(allowed_core_tools)?;
    let mut command = Command::new(qwen_bin);
    crate::services::platform::apply_binary_resolution(&mut command, resolution);
    command
        .args(build_stream_exec_args(prompt, model, &resume_strategy))
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(settings_override) = settings_override.as_ref() {
        command.env(QWEN_CODE_SYSTEM_SETTINGS_ENV, settings_override.path());
    }
    if let Some(channel_id) = report_channel_id {
        command.env(RESTART_REPORT_CHANNEL_ENV, channel_id.to_string());
    }
    if let Some(provider) = report_provider {
        command.env(RESTART_REPORT_PROVIDER_ENV, provider.as_str());
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start Qwen: {}", e))?;

    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(child.id());
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Qwen stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture Qwen stderr".to_string())?;
    let stdout_events = spawn_line_stream_reader(stdout, "Qwen");
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_string(&mut buf);
        buf
    });

    if is_cancelled(cancel_token.as_deref()) {
        kill_child_tree(&mut child);
        let stderr = stderr_handle.join().unwrap_or_default();
        emit_cancellation_error(&sender, String::new(), stderr, None);
        return Ok(QwenAttemptResult::Cancelled);
    }

    let mut state = QwenAttemptState::default();
    match collect_qwen_stream_events(
        &stdout_events,
        cancel_token.as_deref(),
        &mut state,
        QWEN_STREAM_IDLE_TIMEOUT,
        QWEN_STREAM_IDLE_TICKS_BEFORE_RETRY,
    ) {
        QwenStreamLoopResult::Cancelled => {
            kill_child_tree(&mut child);
            let stderr = stderr_handle.join().unwrap_or_default();
            emit_cancellation_error(&sender, state.raw_stdout, stderr, None);
            return Ok(QwenAttemptResult::Cancelled);
        }
        QwenStreamLoopResult::RetrySession { message } => {
            kill_child_tree(&mut child);
            let stderr = stderr_handle.join().unwrap_or_default();
            return Ok(QwenAttemptResult::RetrySession {
                message,
                stdout: state.raw_stdout,
                stderr,
                exit_code: None,
            });
        }
        QwenStreamLoopResult::Eof => {}
    }

    let status = child
        .wait()
        .map_err(|e| format!("Failed waiting for Qwen: {}", e))?;
    let stderr = stderr_handle.join().unwrap_or_default();

    if is_cancelled(cancel_token.as_deref()) {
        emit_cancellation_error(&sender, state.raw_stdout, stderr, status.code());
        return Ok(QwenAttemptResult::Cancelled);
    }

    match finalize_qwen_attempt(&mut state, stderr, status.code()) {
        QwenFinalState::Done { result, session_id } => {
            flush_buffered_stream_messages(&sender, &mut state);
            let _ = sender.send(StreamMessage::Done { result, session_id });
            Ok(QwenAttemptResult::Completed)
        }
        QwenFinalState::Error {
            message,
            stdout,
            stderr,
            exit_code,
        } => {
            flush_buffered_stream_messages(&sender, &mut state);
            let _ = sender.send(StreamMessage::Error {
                message,
                stdout,
                stderr,
                exit_code,
            });
            Ok(QwenAttemptResult::Completed)
        }
        QwenFinalState::RetrySession {
            message,
            stdout,
            stderr,
            exit_code,
        } => Ok(QwenAttemptResult::RetrySession {
            message,
            stdout,
            stderr,
            exit_code,
        }),
    }
}

fn collect_qwen_stream_events(
    stdout_events: &mpsc::Receiver<QwenStreamEvent>,
    cancel_token: Option<&CancelToken>,
    state: &mut QwenAttemptState,
    idle_timeout: Duration,
    idle_ticks_before_retry: u32,
) -> QwenStreamLoopResult {
    let mut idle_ticks = 0;

    loop {
        if is_cancelled(cancel_token) {
            return QwenStreamLoopResult::Cancelled;
        }

        match stdout_events.recv_timeout(idle_timeout) {
            Ok(QwenStreamEvent::Line(line)) => {
                idle_ticks = 0;
                process_qwen_stream_line(&line, state);
            }
            Ok(QwenStreamEvent::ReadError(message)) => {
                return QwenStreamLoopResult::RetrySession { message };
            }
            Ok(QwenStreamEvent::Eof) | Err(RecvTimeoutError::Disconnected) => {
                return QwenStreamLoopResult::Eof;
            }
            Err(RecvTimeoutError::Timeout) => {
                if is_cancelled(cancel_token) {
                    return QwenStreamLoopResult::Cancelled;
                }
                if state.terminal_result_seen {
                    return QwenStreamLoopResult::Eof;
                }
                idle_ticks += 1;
                if idle_ticks >= idle_ticks_before_retry {
                    return QwenStreamLoopResult::RetrySession {
                        message: format!(
                            "Qwen stream produced no output for {} seconds",
                            idle_timeout.as_secs() * idle_ticks_before_retry as u64
                        ),
                    };
                }
            }
        }
    }
}

fn process_qwen_stream_line(line: &str, state: &mut QwenAttemptState) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }

    state.raw_stdout.push_str(trimmed);
    state.raw_stdout.push('\n');

    let Ok(json) = serde_json::from_str::<Value>(trimmed) else {
        return;
    };
    process_qwen_json_event(&json, state);
}

fn process_qwen_json_event(json: &Value, state: &mut QwenAttemptState) {
    match json.get("type").and_then(|v| v.as_str()) {
        Some("system") => {
            if json.get("subtype").and_then(|v| v.as_str()) == Some("session_start") {
                track_session_id(state, json.get("session_id").and_then(|v| v.as_str()));
                state.current_model = json
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        json.get("data")
                            .and_then(|v| v.get("model"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    });
            }
        }
        Some("stream_event") => {
            state.partial_stream_seen = true;
            process_qwen_partial_event(
                json.get("event").unwrap_or(&Value::Null),
                json.get("session_id").and_then(|v| v.as_str()),
                state,
            );
        }
        Some("assistant") => process_qwen_assistant_message(json, state),
        Some("user") => process_qwen_user_message(json, state),
        Some("result") => process_qwen_result_message(json, state),
        _ => {}
    }
}

fn process_qwen_partial_event(
    event: &Value,
    session_id: Option<&str>,
    state: &mut QwenAttemptState,
) {
    track_session_id(state, session_id);

    match event.get("type").and_then(|v| v.as_str()) {
        Some("message_start") => {
            state.current_model = event
                .get("message")
                .and_then(|v| v.get("model"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| state.current_model.clone());
            update_status_from_usage(
                state,
                event.get("message").and_then(|v| v.get("usage")),
                None,
                None,
                None,
            );
        }
        Some("content_block_start") => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let block = event.get("content_block").unwrap_or(&Value::Null);
            let block_type = block
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("text")
                .to_string();
            let input_json = if block_type == "tool_use" {
                block
                    .get("input")
                    .map(render_qwen_value)
                    .filter(|value| value != "{}")
                    .unwrap_or_default()
            } else {
                String::new()
            };
            state.partial_blocks.insert(
                index,
                QwenPartialBlockState {
                    kind: block_type.clone(),
                    tool_name: block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    input_json,
                    thinking_signature: block
                        .get("signature")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    thinking_emitted: false,
                },
            );
            if block_type == "thinking" {
                maybe_emit_partial_thinking(index, state);
            }
        }
        Some("content_block_delta") => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let Some(block_state) = state.partial_blocks.get_mut(&index) else {
                return;
            };
            match event
                .get("delta")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str())
            {
                Some("text_delta") => {
                    let text = event
                        .get("delta")
                        .and_then(|v| v.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !text.is_empty() {
                        state.final_text.push_str(text);
                        state.buffered_messages.push(StreamMessage::Text {
                            content: text.to_string(),
                        });
                    }
                }
                Some("thinking_delta") => {
                    let thinking = event
                        .get("delta")
                        .and_then(|v| v.get("thinking"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !thinking.is_empty() && !block_state.thinking_emitted {
                        let summary = block_state
                            .thinking_signature
                            .clone()
                            .or_else(|| Some("thinking".to_string()));
                        state
                            .buffered_messages
                            .push(StreamMessage::Thinking { summary });
                        block_state.thinking_emitted = true;
                    }
                }
                Some("signature_delta") => {
                    let signature = event
                        .get("delta")
                        .and_then(|v| v.get("signature"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !signature.is_empty() {
                        block_state
                            .thinking_signature
                            .get_or_insert_with(String::new)
                            .push_str(signature);
                    }
                }
                Some("input_json_delta") => {
                    let partial_json = event
                        .get("delta")
                        .and_then(|v| v.get("partial_json"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !partial_json.is_empty() {
                        block_state.input_json.push_str(partial_json);
                    }
                }
                _ => {}
            }
        }
        Some("content_block_stop") => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(block_state) = state.partial_blocks.remove(&index) {
                if block_state.kind == "tool_use" {
                    state.buffered_messages.push(StreamMessage::ToolUse {
                        name: block_state.tool_name.unwrap_or_else(|| "tool".to_string()),
                        input: normalize_tool_input(block_state.input_json),
                    });
                } else if block_state.kind == "thinking" && !block_state.thinking_emitted {
                    state.buffered_messages.push(StreamMessage::Thinking {
                        summary: block_state.thinking_signature,
                    });
                }
            }
        }
        Some("message_delta") => {
            update_status_from_usage(state, event.get("usage"), None, None, None);
        }
        Some("tool_progress") | Some("message_stop") => {}
        _ => {}
    }
}

fn process_qwen_assistant_message(json: &Value, state: &mut QwenAttemptState) {
    track_session_id(state, json.get("session_id").and_then(|v| v.as_str()));

    let message = json.get("message").unwrap_or(&Value::Null);
    if message.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return;
    }

    state.current_model = message
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| state.current_model.clone());
    update_status_from_usage(state, message.get("usage"), None, None, None);

    let Some(content) = message.get("content").and_then(|v| v.as_array()) else {
        return;
    };

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                if !state.partial_stream_seen {
                    state.final_text.push_str(text);
                    state.buffered_messages.push(StreamMessage::Text {
                        content: text.to_string(),
                    });
                } else if state.final_text.is_empty() {
                    state.final_text.push_str(text);
                }
            }
            Some("thinking") if !state.partial_stream_seen => {
                let summary = block
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        block
                            .get("thinking")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(|value| value.chars().take(80).collect())
                    });
                state
                    .buffered_messages
                    .push(StreamMessage::Thinking { summary });
            }
            Some("tool_use") if !state.partial_stream_seen => {
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let input = block
                    .get("input")
                    .map(render_qwen_value)
                    .map(normalize_tool_input)
                    .unwrap_or_else(|| "{}".to_string());
                state
                    .buffered_messages
                    .push(StreamMessage::ToolUse { name, input });
            }
            _ => {}
        }
    }
}

fn process_qwen_user_message(json: &Value, state: &mut QwenAttemptState) {
    let Some(content) = json
        .get("message")
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_array())
    else {
        return;
    };

    for block in content {
        if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
            continue;
        }
        let content = block
            .get("content")
            .map(render_qwen_value)
            .or_else(|| block.get("error").map(render_qwen_value))
            .unwrap_or_default();
        let is_error = block
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        state
            .buffered_messages
            .push(StreamMessage::ToolResult { content, is_error });
    }
}

fn process_qwen_result_message(json: &Value, state: &mut QwenAttemptState) {
    track_session_id(state, json.get("session_id").and_then(|v| v.as_str()));
    state.terminal_result_seen = true;

    let duration_ms = json.get("duration_ms").and_then(|v| v.as_u64());
    let num_turns = json
        .get("num_turns")
        .and_then(|v| v.as_u64())
        .and_then(|value| u32::try_from(value).ok());
    update_status_from_usage(state, json.get("usage"), duration_ms, num_turns, None);

    state.buffered_messages.push(StreamMessage::StatusUpdate {
        model: state.current_model.clone(),
        cost_usd: None,
        total_cost_usd: None,
        duration_ms: state.status.duration_ms,
        num_turns: state.status.num_turns,
        input_tokens: state.status.input_tokens,
        cache_create_tokens: state.status.cache_create_tokens,
        cache_read_tokens: state.status.cache_read_tokens,
        output_tokens: state.status.output_tokens,
    });

    let is_error = json
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_error {
        state.last_error_message = json
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                json.get("subtype")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
        return;
    }

    state.terminal_result_text = json
        .get("result")
        .map(render_qwen_value)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
}

fn finalize_qwen_attempt(
    state: &mut QwenAttemptState,
    stderr: String,
    exit_code: Option<i32>,
) -> QwenFinalState {
    let final_text = std::mem::take(&mut state.final_text);
    let raw_stdout = std::mem::take(&mut state.raw_stdout);
    let last_session_id = state.last_session_id.take();
    let last_error_message = state.last_error_message.take();
    let terminal_result_seen = state.terminal_result_seen;
    let terminal_result_text = state.terminal_result_text.take();

    if terminal_result_seen {
        if let Some(message) = last_error_message {
            return QwenFinalState::Error {
                message,
                stdout: raw_stdout,
                stderr,
                exit_code,
            };
        }

        let result = final_text.trim().to_string();
        let result = if result.is_empty() {
            terminal_result_text.unwrap_or_default()
        } else {
            result
        };
        if result.is_empty() {
            return QwenFinalState::Error {
                message: "Qwen emitted a terminal result without any response text".to_string(),
                stdout: raw_stdout,
                stderr,
                exit_code,
            };
        }
        return QwenFinalState::Done {
            result,
            session_id: last_session_id,
        };
    }

    if let Some(message) = last_error_message {
        return QwenFinalState::Error {
            message,
            stdout: raw_stdout,
            stderr,
            exit_code,
        };
    }

    if exit_code.unwrap_or(0) != 0 {
        return QwenFinalState::RetrySession {
            message: derive_error_message(&raw_stdout, &stderr, exit_code, "Qwen"),
            stdout: raw_stdout,
            stderr,
            exit_code,
        };
    }

    if !stderr.trim().is_empty() {
        return QwenFinalState::Error {
            message: derive_error_message(&raw_stdout, &stderr, exit_code, "Qwen"),
            stdout: raw_stdout,
            stderr,
            exit_code,
        };
    }

    QwenFinalState::RetrySession {
        message: QWEN_SESSION_DEAD_MESSAGE.to_string(),
        stdout: raw_stdout,
        stderr,
        exit_code,
    }
}

fn flush_buffered_stream_messages(sender: &Sender<StreamMessage>, state: &mut QwenAttemptState) {
    for message in state.buffered_messages.drain(..) {
        let _ = sender.send(message);
    }
}

fn emit_cancellation_error(
    sender: &Sender<StreamMessage>,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
) {
    let _ = sender.send(StreamMessage::Error {
        message: QWEN_CANCELLED_MESSAGE.to_string(),
        stdout,
        stderr,
        exit_code,
    });
}

fn is_cancelled(token: Option<&CancelToken>) -> bool {
    token
        .map(|token| token.cancelled.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false)
}

fn remote_profile_not_supported_message() -> String {
    "NotSupported: Qwen provider does not support remote execution yet. Remove `remote_profile` or use a provider with remote support.".to_string()
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn execute_streaming_local_tmux(
    prompt: &str,
    model: Option<&str>,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    tmux_session_name: &str,
    qwen_resolution: &crate::services::platform::BinaryResolution,
    allowed_core_tools: Option<&[String]>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
) -> Result<(), String> {
    let output_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let input_fifo_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "input");
    let prompt_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "prompt");
    let owner_path = tmux_owner_path(tmux_session_name);

    let session_exists = tmux_session_exists(tmux_session_name);
    let session_usable = tmux_session_has_live_pane(tmux_session_name)
        && std::fs::metadata(&output_path).is_ok()
        && std::path::Path::new(&input_fifo_path).exists();

    if session_usable {
        match send_followup_to_tmux(
            prompt,
            &output_path,
            &input_fifo_path,
            sender.clone(),
            cancel_token.clone(),
            tmux_session_name,
        )? {
            FollowupResult::Delivered => return Ok(()),
            FollowupResult::RecreateSession { error } => {
                record_tmux_exit_reason(
                    tmux_session_name,
                    &format!("followup failed, recreating: {}", error),
                );
                crate::services::platform::tmux::kill_session(tmux_session_name);
            }
        }
    } else if session_exists {
        record_tmux_exit_reason(
            tmux_session_name,
            "stale local session cleanup before recreate",
        );
        crate::services::platform::tmux::kill_session(tmux_session_name);
    }

    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&input_fifo_path);
    let _ = std::fs::remove_file(&prompt_path);
    let _ = std::fs::remove_file(&owner_path);
    let _ = std::fs::remove_file(crate::services::tmux_common::session_temp_path(
        tmux_session_name,
        "sh",
    ));

    std::fs::write(&output_path, "").map_err(|e| format!("Failed to create output file: {}", e))?;

    let mkfifo = Command::new("mkfifo")
        .arg(&input_fifo_path)
        .output()
        .map_err(|e| format!("Failed to create input FIFO: {}", e))?;
    if !mkfifo.status.success() {
        let _ = std::fs::remove_file(&output_path);
        return Err(format!(
            "mkfifo failed: {}",
            String::from_utf8_lossy(&mkfifo.stderr)
        ));
    }

    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;
    write_tmux_owner_marker(tmux_session_name)?;

    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
    let qwen_bin = qwen_resolution
        .resolved_path
        .as_deref()
        .ok_or_else(|| "Qwen CLI not found".to_string())?;
    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");

    let mut env_lines = String::new();
    if let Some(exec_path) = qwen_resolution.exec_path.as_deref() {
        env_lines.push_str(&format!(
            "export PATH='{}'\n",
            exec_path.replace('\'', "'\\''")
        ));
    }
    if let Ok(root_dir) = std::env::var("AGENTDESK_ROOT_DIR") {
        let trimmed = root_dir.trim();
        if !trimmed.is_empty() {
            env_lines.push_str(&format!(
                "export AGENTDESK_ROOT_DIR='{}'\n",
                trimmed.replace('\'', "'\\''")
            ));
        }
    }
    if let Some(channel_id) = report_channel_id {
        env_lines.push_str(&format!(
            "export {}={}\n",
            RESTART_REPORT_CHANNEL_ENV, channel_id
        ));
    }
    if let Some(provider) = report_provider {
        env_lines.push_str(&format!(
            "export {}={}\n",
            RESTART_REPORT_PROVIDER_ENV,
            provider.as_str()
        ));
    }

    let script_content = format!(
        "#!/bin/bash\n\
        {env}\
        exec {exe} qwen-tmux-wrapper \\\n  \
        --output-file {output} \\\n  \
        --input-fifo {input_fifo} \\\n  \
        --prompt-file {prompt} \\\n  \
        --cwd {wd} \\\n  \
        --qwen-bin {qwen_bin}{model_arg}{resume_arg}{core_tool_args}\n",
        env = env_lines,
        exe = shell_escape(&exe.display().to_string()),
        output = shell_escape(&output_path),
        input_fifo = shell_escape(&input_fifo_path),
        prompt = shell_escape(&prompt_path),
        wd = shell_escape(working_dir),
        qwen_bin = shell_escape(qwen_bin),
        model_arg = model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!(" \\\n  --qwen-model {}", shell_escape(value)))
            .unwrap_or_default(),
        resume_arg = session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!(" \\\n  --resume-session-id {}", shell_escape(value)))
            .unwrap_or_default(),
        core_tool_args = allowed_core_tools
            .map(|tools| {
                tools
                    .iter()
                    .map(|tool| format!(" \\\n  --qwen-core-tool {}", shell_escape(tool)))
                    .collect::<String>()
            })
            .unwrap_or_default(),
    );

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write launch script: {}", e))?;

    let tmux_result = crate::services::platform::tmux::create_session(
        tmux_session_name,
        Some(working_dir),
        &format!("bash {}", shell_escape(&script_path)),
    )?;

    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        let _ = std::fs::remove_file(&output_path);
        let _ = std::fs::remove_file(&input_fifo_path);
        let _ = std::fs::remove_file(&prompt_path);
        let _ = std::fs::remove_file(&owner_path);
        let _ = std::fs::remove_file(&script_path);
        return Err(format!("tmux error: {}", stderr));
    }

    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    let gen_marker_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
    let current_gen = crate::services::discord::runtime_store::load_generation();
    let _ = std::fs::write(&gen_marker_path, current_gen.to_string());

    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap() = Some(tmux_session_name.to_string());
    }

    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token,
        SessionProbe::tmux(tmux_session_name.to_string()),
    )?;

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        }
        ReadOutputResult::SessionDied { .. } => {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 세션이 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
        }
    }

    Ok(())
}

#[cfg(unix)]
fn send_followup_to_tmux(
    prompt: &str,
    output_path: &str,
    input_fifo_path: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<FollowupResult, String> {
    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .open(input_fifo_path)
        .map_err(|e| format!("Failed to open input FIFO: {}", e))
        .and_then(|mut fifo| {
            let encoded = format!(
                "{}{}",
                TMUX_PROMPT_B64_PREFIX,
                BASE64_STANDARD.encode(prompt.as_bytes())
            );
            writeln!(fifo, "{}", encoded)
                .map_err(|e| format!("Failed to write to input FIFO: {}", e))?;
            fifo.flush()
                .map_err(|e| format!("Failed to flush input FIFO: {}", e))?;
            Ok(())
        });

    if let Err(e) = write_result {
        if should_recreate_session_after_followup_fifo_error(&e) {
            return Ok(FollowupResult::RecreateSession { error: e });
        }
        return Err(e);
    }

    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap() = Some(tmux_session_name.to_string());
    }

    let read_result = match read_output_file_until_result_tracked(
        output_path,
        start_offset,
        sender.clone(),
        cancel_token,
        SessionProbe::tmux(tmux_session_name.to_string()),
    ) {
        Ok(read_result) => read_result,
        Err(failure) => {
            let output_exists = std::fs::metadata(output_path).is_ok();
            let current_file_len = std::fs::metadata(output_path).ok().map(|meta| meta.len());
            let input_exists = std::path::Path::new(input_fifo_path).exists();
            let session_alive = tmux_session_has_live_pane(tmux_session_name);
            let ready_for_input = session_alive
                && crate::services::provider::tmux_session_ready_for_input(tmux_session_name);

            if let Some(fallback) = tmux_followup_fallback_after_read_error(
                start_offset,
                failure.last_offset,
                current_file_len,
                session_alive,
                ready_for_input,
                output_exists,
                input_exists,
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ qwen follow-up read failed for {tmux_session_name}: {}; attaching fallback watcher at offset {} (ready_for_input={}, emit_done={})",
                    failure.error,
                    fallback.last_offset,
                    ready_for_input,
                    fallback.emit_synthetic_done
                );
                if fallback.emit_synthetic_done {
                    let _ = sender.send(StreamMessage::Done {
                        result: String::new(),
                        session_id: None,
                    });
                }
                let _ = sender.send(StreamMessage::TmuxReady {
                    output_path: output_path.to_string(),
                    input_fifo_path: input_fifo_path.to_string(),
                    tmux_session_name: tmux_session_name.to_string(),
                    last_offset: fallback.last_offset,
                });
                return Ok(FollowupResult::Delivered);
            }

            if !session_alive {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ qwen follow-up read failed and tmux session died for {tmux_session_name}: {}; recreating session",
                    failure.error
                );
                return Ok(FollowupResult::RecreateSession {
                    error: failure.error,
                });
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::error!(
                "  [{ts}] ✗ qwen follow-up read failed with no watcher fallback for {tmux_session_name}: {} (output_exists={}, input_exists={})",
                failure.error,
                output_exists,
                input_exists
            );
            return Err(failure.error);
        }
    };

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path: output_path.to_string(),
                input_fifo_path: input_fifo_path.to_string(),
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
            Ok(FollowupResult::Delivered)
        }
        ReadOutputResult::SessionDied { .. } => Ok(FollowupResult::RecreateSession {
            error: "session died during follow-up output reading".to_string(),
        }),
    }
}

fn execute_streaming_local_process(
    prompt: &str,
    model: Option<&str>,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    session_name: &str,
    qwen_resolution: &crate::services::platform::BinaryResolution,
    allowed_core_tools: Option<&[String]>,
) -> Result<(), String> {
    use crate::services::session_backend::{ProcessBackend, SessionBackend, SessionConfig};

    let output_path = format!(
        "{}/agentdesk-{}.jsonl",
        std::env::temp_dir().display(),
        session_name
    );
    let prompt_path = format!(
        "{}/agentdesk-{}.prompt",
        std::env::temp_dir().display(),
        session_name
    );

    if process_session_is_alive(session_name) {
        let start_offset = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let encoded = format!(
            "{}{}",
            TMUX_PROMPT_B64_PREFIX,
            BASE64_STANDARD.encode(prompt.as_bytes())
        );
        send_process_session_input(session_name, &encoded)?;
        let read_result = read_output_file_until_result(
            &output_path,
            start_offset,
            sender.clone(),
            cancel_token,
            process_session_probe(session_name),
        )?;

        match read_result {
            ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
                let _ = sender.send(StreamMessage::ProcessReady {
                    output_path: output_path.to_string(),
                    session_name: session_name.to_string(),
                    last_offset: offset,
                });
            }
            ReadOutputResult::SessionDied { .. } => {
                let _ = sender.send(StreamMessage::Done {
                    result: "⚠ 세션이 종료되었습니다.".to_string(),
                    session_id: None,
                });
                remove_process_session(session_name);
            }
        }
        return Ok(());
    }

    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&prompt_path);
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;

    let qwen_bin = qwen_resolution
        .resolved_path
        .as_deref()
        .ok_or_else(|| "Qwen CLI not found".to_string())?;
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;

    let config = SessionConfig {
        session_name: session_name.to_string(),
        working_dir: working_dir.to_string(),
        agentdesk_exe: exe.display().to_string(),
        output_path: output_path.clone(),
        prompt_path: prompt_path.clone(),
        wrapper_subcommand: "qwen-tmux-wrapper".to_string(),
        wrapper_args: {
            let mut args = vec!["--qwen-bin".to_string(), qwen_bin.to_string()];
            if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
                args.push("--qwen-model".to_string());
                args.push(model.to_string());
            }
            if let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) {
                args.push("--resume-session-id".to_string());
                args.push(session_id.to_string());
            }
            if let Some(core_tools) = allowed_core_tools {
                for tool in core_tools {
                    args.push("--qwen-core-tool".to_string());
                    args.push(tool.to_string());
                }
            }
            args
        },
        env_vars: qwen_resolution
            .exec_path
            .as_ref()
            .map(|exec_path| vec![("PATH".to_string(), exec_path.clone())])
            .unwrap_or_default(),
    };

    let backend = ProcessBackend::new();
    let handle = backend.create_session(&config)?;

    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(handle.pid());
    }

    insert_process_session(session_name.to_string(), handle);

    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token,
        process_session_probe(session_name),
    )?;

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            let _ = sender.send(StreamMessage::ProcessReady {
                output_path,
                session_name: session_name.to_string(),
                last_offset: offset,
            });
        }
        ReadOutputResult::SessionDied { .. } => {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 프로세스가 종료되었습니다.".to_string(),
                session_id: None,
            });
            remove_process_session(session_name);
        }
    }

    Ok(())
}

fn compose_qwen_prompt(
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> String {
    let mut sections = Vec::new();

    if let Some(system_prompt) = system_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!(
            "[Authoritative Instructions]\n{}\n\nThese instructions are authoritative for this turn. Follow them over any generic assistant persona unless the user explicitly asks to inspect or compare them.",
            system_prompt
        ));
    }

    if let Some(allowed_tools) = allowed_tools.filter(|tools| !tools.is_empty()) {
        sections.push(format!(
            "[Tool Policy]\nIf tools are needed, stay within this allowlist unless the user explicitly asks to change it: {}",
            allowed_tools.join(", ")
        ));
    }

    if sections.is_empty() {
        return prompt.to_string();
    }

    sections.push(format!("[User Request]\n{}", prompt));
    sections.join("\n\n")
}

fn map_allowed_tool_to_qwen_core_tools(tool: &str) -> Option<&'static [&'static str]> {
    match resolve_shared_allowed_tool_compat(tool)? {
        SharedAllowedToolKind::Bash => Some(&["run_shell_command"]),
        SharedAllowedToolKind::Read => Some(&["read_file"]),
        SharedAllowedToolKind::Edit => Some(&["edit"]),
        SharedAllowedToolKind::Write => Some(&["write_file"]),
        SharedAllowedToolKind::Glob => Some(&["glob"]),
        SharedAllowedToolKind::Grep => Some(&["grep_search"]),
        SharedAllowedToolKind::Task => Some(&["agent"]),
        SharedAllowedToolKind::WebFetch => Some(&["web_fetch"]),
        SharedAllowedToolKind::WebSearch => Some(&["web_search"]),
        SharedAllowedToolKind::Skill => Some(&["skill"]),
        SharedAllowedToolKind::AskUserQuestion => Some(&["ask_user_question"]),
        SharedAllowedToolKind::PlanMode => Some(&["exit_plan_mode"]),
    }
}

pub(crate) fn resolve_allowed_core_tools(
    allowed_tools: Option<&[String]>,
) -> Result<Option<Vec<String>>, String> {
    let Some(allowed_tools) = allowed_tools.filter(|tools| !tools.is_empty()) else {
        return Ok(None);
    };

    let mut unsupported = Vec::new();
    let mut seen = HashSet::new();
    let mut core_tools = Vec::new();

    for tool in allowed_tools {
        match map_allowed_tool_to_qwen_core_tools(tool) {
            Some(mapped_tools) => {
                for core_tool in mapped_tools {
                    if seen.insert(*core_tool) {
                        core_tools.push((*core_tool).to_string());
                    }
                }
            }
            None => unsupported.push(tool.trim().to_string()),
        }
    }

    if !unsupported.is_empty() {
        return Err(format!(
            "InvalidArgument: Qwen provider does not support these allowed_tools: {}. Supported with Qwen: {}",
            unsupported.join(", "),
            QWEN_SUPPORTED_ALLOWED_TOOLS.join(", "),
        ));
    }

    if core_tools.is_empty() {
        return Err(format!(
            "InvalidArgument: Qwen allowed_tools resolved to an empty tools.core allowlist. Supported with Qwen: {}",
            QWEN_SUPPORTED_ALLOWED_TOOLS.join(", "),
        ));
    }

    Ok(Some(core_tools))
}

pub(crate) fn create_system_settings_override(
    allowed_core_tools: Option<&[String]>,
) -> Result<Option<QwenSystemSettingsOverride>, String> {
    let Some(allowed_core_tools) = allowed_core_tools.filter(|tools| !tools.is_empty()) else {
        return Ok(None);
    };

    let settings_dir = std::env::temp_dir().join("agentdesk-qwen");
    std::fs::create_dir_all(&settings_dir)
        .map_err(|e| format!("Failed to create Qwen settings temp dir: {}", e))?;

    let path = settings_dir.join(format!("system-settings-{}.json", Uuid::new_v4()));
    let payload = json!({
        "tools": {
            "core": allowed_core_tools,
        }
    });
    let content = serde_json::to_vec_pretty(&payload)
        .map_err(|e| format!("Failed to serialize Qwen system settings: {}", e))?;
    std::fs::write(&path, content)
        .map_err(|e| format!("Failed to write Qwen system settings override: {}", e))?;

    Ok(Some(QwenSystemSettingsOverride { path }))
}

fn build_simple_exec_args(prompt: &str) -> Vec<String> {
    vec![
        "-p".to_string(),
        prompt.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--approval-mode".to_string(),
        "yolo".to_string(),
        "--sandbox".to_string(),
        "false".to_string(),
    ]
}

pub(crate) fn build_stream_exec_args(
    prompt: &str,
    model: Option<&str>,
    resume_strategy: &QwenResumeStrategy,
) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    match resume_strategy {
        QwenResumeStrategy::Fresh => {}
        QwenResumeStrategy::Continue => {
            args.push("--continue".to_string());
        }
        QwenResumeStrategy::Resume(session_id) => {
            args.push("--resume".to_string());
            args.push(session_id.clone());
        }
    }

    args.push("-p".to_string());
    args.push(prompt.to_string());
    args.push("--output-format".to_string());
    args.push("stream-json".to_string());
    args.push("--include-partial-messages".to_string());
    args.push("--approval-mode".to_string());
    args.push("yolo".to_string());
    args.push("--sandbox".to_string());
    args.push("false".to_string());
    args
}

pub(crate) fn normalize_resume_strategy(
    session_id: Option<&str>,
    working_dir: &str,
) -> Result<QwenResumeStrategy, String> {
    let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        if has_prior_qwen_chat_cache(working_dir) {
            return Ok(QwenResumeStrategy::Continue);
        }
        return Ok(QwenResumeStrategy::Fresh);
    };

    if !is_valid_session_id(session_id) {
        return Err(
            "InvalidArgument: Qwen session_id must use a resumable token produced by the CLI"
                .to_string(),
        );
    }

    Ok(QwenResumeStrategy::Resume(session_id.to_string()))
}

fn has_prior_qwen_chat_cache(working_dir: &str) -> bool {
    let Some(chats_dir) = qwen_chat_cache_dir_for_working_dir(working_dir) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(chats_dir) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_file()
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
    })
}

fn qwen_chat_cache_dir_for_working_dir(working_dir: &str) -> Option<PathBuf> {
    let home = qwen_home_dir()?;
    Some(
        home.join(".qwen")
            .join("projects")
            .join(qwen_project_cache_key(working_dir))
            .join("chats"),
    )
}

pub(crate) fn qwen_project_cache_key(working_dir: &str) -> String {
    normalize_qwen_working_dir(working_dir)
        .to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn normalize_qwen_working_dir(working_dir: &str) -> PathBuf {
    let expanded = expand_home_dir(working_dir);
    std::fs::canonicalize(&expanded).unwrap_or(expanded)
}

fn qwen_home_dir() -> Option<PathBuf> {
    std::env::var_os("QWEN_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(dirs::home_dir)
}

fn expand_home_dir(path: &str) -> PathBuf {
    if path == "~" {
        return qwen_home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = qwen_home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

fn is_valid_session_id(session_id: &str) -> bool {
    static SESSION_ID_RE: OnceLock<Regex> = OnceLock::new();
    !session_id.is_empty()
        && session_id.len() <= 128
        && SESSION_ID_RE
            .get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:-]*$").unwrap())
            .is_match(session_id)
}

fn track_session_id(state: &mut QwenAttemptState, session_id: Option<&str>) {
    let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    if !is_valid_session_id(session_id) {
        return;
    }
    state.last_session_id = Some(session_id.to_string());
    if !state
        .buffered_messages
        .iter()
        .any(|message| matches!(message, StreamMessage::Init { .. }))
    {
        state.buffered_messages.push(StreamMessage::Init {
            session_id: session_id.to_string(),
        });
    }
}

fn update_status_from_usage(
    state: &mut QwenAttemptState,
    usage: Option<&Value>,
    duration_ms: Option<u64>,
    num_turns: Option<u32>,
    model: Option<String>,
) {
    if let Some(model) = model {
        state.status.model = Some(model);
    }
    if let Some(model) = state.current_model.clone() {
        state.status.model = Some(model);
    }
    if let Some(duration_ms) = duration_ms {
        state.status.duration_ms = Some(duration_ms);
    }
    if let Some(num_turns) = num_turns {
        state.status.num_turns = Some(num_turns);
    }

    let Some(usage) = usage else {
        return;
    };
    if let Some(input_tokens) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
        state.status.input_tokens = Some(input_tokens);
    }
    if let Some(cache_create_tokens) = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
    {
        state.status.cache_create_tokens = Some(cache_create_tokens);
    }
    if let Some(cache_read_tokens) = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
    {
        state.status.cache_read_tokens = Some(cache_read_tokens);
    }
    if let Some(output_tokens) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
        state.status.output_tokens = Some(output_tokens);
    }
}

fn maybe_emit_partial_thinking(index: usize, state: &mut QwenAttemptState) {
    let Some(block_state) = state.partial_blocks.get_mut(&index) else {
        return;
    };
    if block_state.kind != "thinking" || block_state.thinking_emitted {
        return;
    }
    state.buffered_messages.push(StreamMessage::Thinking {
        summary: block_state
            .thinking_signature
            .clone()
            .or_else(|| Some("thinking".to_string())),
    });
    block_state.thinking_emitted = true;
}

fn normalize_tool_input(raw: String) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "{}".to_string()
    } else {
        trimmed.to_string()
    }
}

fn extract_text_from_json_output(output: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return String::new();
    };
    let Some(messages) = value.as_array() else {
        return String::new();
    };

    let mut text = String::new();
    let mut result_text = None;
    for message in messages {
        match message.get("type").and_then(|v| v.as_str()) {
            Some("assistant") => {
                let Some(content) = message
                    .get("message")
                    .and_then(|v| v.get("content"))
                    .and_then(|v| v.as_array())
                else {
                    continue;
                };
                for block in content {
                    if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(fragment) = block.get("text").and_then(|v| v.as_str()) {
                            text.push_str(fragment);
                        }
                    }
                }
            }
            Some("result")
                if !message
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false) =>
            {
                result_text = message
                    .get("result")
                    .map(render_qwen_value)
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty());
            }
            _ => {}
        }
    }

    let text = text.trim().to_string();
    if text.is_empty() {
        result_text.unwrap_or_default()
    } else {
        text
    }
}

fn derive_error_message(stdout: &str, stderr: &str, exit_code: Option<i32>, label: &str) -> String {
    if !stderr.trim().is_empty() {
        return stderr.trim().to_string();
    }

    if let Ok(value) = serde_json::from_str::<Value>(stdout) {
        if let Some(messages) = value.as_array() {
            for message in messages.iter().rev() {
                if message.get("type").and_then(|v| v.as_str()) == Some("result")
                    && message.get("is_error").and_then(|v| v.as_bool()) == Some(true)
                {
                    if let Some(error) = message
                        .get("error")
                        .and_then(|v| v.get("message"))
                        .and_then(|v| v.as_str())
                    {
                        if !error.trim().is_empty() {
                            return error.trim().to_string();
                        }
                    }
                }
            }
        }
    }

    if let Some(last) = stdout.lines().rev().find(|line| !line.trim().is_empty()) {
        return last.trim().to_string();
    }

    format!("{} exited with code {:?}", label, exit_code)
}

fn render_qwen_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        Value::String(v) => v.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        QWEN_CODE_SYSTEM_SETTINGS_ENV, QwenAttemptState, QwenResumeStrategy, QwenStreamEvent,
        QwenStreamLoopResult, build_simple_exec_args, build_stream_exec_args,
        collect_qwen_stream_events, compose_qwen_prompt, create_system_settings_override,
        extract_text_from_json_output, normalize_resume_strategy, process_qwen_json_event,
        qwen_project_cache_key, resolve_allowed_core_tools,
    };
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::provider::CancelToken;
    use serde_json::json;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;
    use tempfile::TempDir;

    fn with_temp_qwen_home<F>(f: F)
    where
        F: FnOnce(&TempDir, &TempDir),
    {
        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp_home = TempDir::new().unwrap();
        let temp_project = TempDir::new().unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");

        unsafe {
            std::env::set_var("HOME", temp_home.path());
            std::env::set_var("USERPROFILE", temp_home.path());
        }

        f(&temp_home, &temp_project);

        match prev_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_userprofile {
            Some(value) => unsafe { std::env::set_var("USERPROFILE", value) },
            None => unsafe { std::env::remove_var("USERPROFILE") },
        }
    }

    fn create_prior_qwen_chat_cache(temp_home: &TempDir, working_dir: &TempDir) {
        let chats_dir = temp_home
            .path()
            .join(".qwen")
            .join("projects")
            .join(qwen_project_cache_key(working_dir.path().to_str().unwrap()))
            .join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(chats_dir.join("turn-1.jsonl"), "{\"type\":\"result\"}\n").unwrap();
    }

    #[test]
    fn compose_qwen_prompt_includes_authoritative_sections() {
        let prompt = compose_qwen_prompt(
            "사용자 요청",
            Some("시스템 지침"),
            Some(&["Bash".to_string(), "Read".to_string()]),
        );
        assert!(prompt.contains("[Authoritative Instructions]"));
        assert!(prompt.contains("[Tool Policy]"));
        assert!(prompt.contains("[User Request]"));
    }

    #[test]
    fn build_stream_exec_args_prefers_resume_session() {
        let args = build_stream_exec_args(
            "hello",
            Some("qwen3-coder"),
            &QwenResumeStrategy::Resume("session-123".to_string()),
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--resume", "session-123"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "qwen3-coder"])
        );
        assert!(args.contains(&"--include-partial-messages".to_string()));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--approval-mode", "yolo"])
        );
        assert!(!args.contains(&"-y".to_string()));
    }

    #[test]
    fn build_simple_exec_args_uses_approval_mode_yolo() {
        let args = build_simple_exec_args("hello");

        assert!(
            args.windows(2)
                .any(|pair| pair == ["--approval-mode", "yolo"])
        );
        assert!(!args.contains(&"-y".to_string()));
        assert!(args.windows(2).any(|pair| pair == ["--sandbox", "false"]));
    }

    #[test]
    fn resolve_allowed_core_tools_maps_supported_tools() {
        let tools = resolve_allowed_core_tools(Some(&[
            "Bash".to_string(),
            "Read".to_string(),
            "WebSearch".to_string(),
            "Bash".to_string(),
        ]))
        .unwrap()
        .unwrap();

        assert_eq!(
            tools,
            vec![
                "run_shell_command".to_string(),
                "read_file".to_string(),
                "web_search".to_string()
            ]
        );
    }

    #[test]
    fn resolve_allowed_core_tools_accepts_shared_agentdesk_aliases() {
        let tools = resolve_allowed_core_tools(Some(&[
            "TaskOutput".to_string(),
            "TaskStop".to_string(),
            "NotebookEdit".to_string(),
            "Skill".to_string(),
            "TaskCreate".to_string(),
            "TaskGet".to_string(),
            "TaskUpdate".to_string(),
            "TaskList".to_string(),
            "AskUserQuestion".to_string(),
            "EnterPlanMode".to_string(),
        ]))
        .unwrap()
        .unwrap();

        assert_eq!(
            tools,
            vec![
                "agent".to_string(),
                "edit".to_string(),
                "skill".to_string(),
                "ask_user_question".to_string(),
                "exit_plan_mode".to_string(),
            ]
        );
    }

    #[test]
    fn collect_qwen_stream_events_returns_cancelled_when_token_flips_mid_stream() {
        let token = Arc::new(CancelToken::new());
        let (tx, rx) = mpsc::channel();
        let mut state = QwenAttemptState::default();

        tx.send(QwenStreamEvent::Line(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"partial"}]}}"#
                .to_string(),
        ))
        .unwrap();
        let token_for_thread = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            token_for_thread.cancelled.store(true, Ordering::Relaxed);
        });

        let result = collect_qwen_stream_events(
            &rx,
            Some(token.as_ref()),
            &mut state,
            Duration::from_millis(100),
            2,
        );

        assert_eq!(result, QwenStreamLoopResult::Cancelled);
        assert_eq!(state.final_text, "partial");
        assert!(!state.raw_stdout.is_empty());
    }

    #[test]
    fn resolve_allowed_core_tools_rejects_unknown_tools() {
        let err = resolve_allowed_core_tools(Some(&[
            "Bash".to_string(),
            "DefinitelyUnsupported".to_string(),
        ]))
        .unwrap_err();

        assert!(err.contains("DefinitelyUnsupported"));
        assert!(err.contains("Supported with Qwen"));
    }

    #[test]
    fn create_system_settings_override_writes_tools_core() {
        let override_file = create_system_settings_override(Some(&[
            "run_shell_command".to_string(),
            "read_file".to_string(),
        ]))
        .unwrap()
        .unwrap();

        let content = std::fs::read_to_string(override_file.path()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            json,
            json!({
                "tools": {
                    "core": ["run_shell_command", "read_file"]
                }
            })
        );
        assert_eq!(
            QWEN_CODE_SYSTEM_SETTINGS_ENV,
            "QWEN_CODE_SYSTEM_SETTINGS_PATH"
        );
    }

    #[test]
    fn normalize_resume_strategy_defaults_to_fresh_without_prior_cache() {
        with_temp_qwen_home(|_temp_home, working_dir| {
            assert!(matches!(
                normalize_resume_strategy(None, working_dir.path().to_str().unwrap()).unwrap(),
                QwenResumeStrategy::Fresh
            ));
        });
    }

    #[test]
    fn normalize_resume_strategy_uses_continue_with_prior_cache() {
        with_temp_qwen_home(|temp_home, working_dir| {
            create_prior_qwen_chat_cache(temp_home, working_dir);
            assert!(matches!(
                normalize_resume_strategy(None, working_dir.path().to_str().unwrap()).unwrap(),
                QwenResumeStrategy::Continue
            ));
        });
    }

    #[test]
    fn extract_text_from_json_output_prefers_assistant_content() {
        let output = json!([
            {
                "type": "assistant",
                "message": {
                    "content": [
                        { "type": "text", "text": "Hello " },
                        { "type": "text", "text": "Qwen" }
                    ]
                }
            },
            {
                "type": "result",
                "is_error": false,
                "result": "Hello Qwen"
            }
        ]);
        assert_eq!(
            extract_text_from_json_output(&output.to_string()),
            "Hello Qwen"
        );
    }

    #[test]
    fn process_qwen_json_event_maps_partial_tool_use() {
        let mut state = QwenAttemptState::default();
        process_qwen_json_event(
            &json!({
                "type": "stream_event",
                "session_id": "session-123",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "Bash",
                        "input": {}
                    }
                }
            }),
            &mut state,
        );
        process_qwen_json_event(
            &json!({
                "type": "stream_event",
                "session_id": "session-123",
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"cmd\":\"pwd\"}"
                    }
                }
            }),
            &mut state,
        );
        process_qwen_json_event(
            &json!({
                "type": "stream_event",
                "session_id": "session-123",
                "event": {
                    "type": "content_block_stop",
                    "index": 0
                }
            }),
            &mut state,
        );

        assert!(matches!(
            state.buffered_messages.first(),
            Some(StreamMessage::Init { session_id }) if session_id == "session-123"
        ));
        assert!(matches!(
            state.buffered_messages.last(),
            Some(StreamMessage::ToolUse { name, input })
                if name == "Bash" && input == "{\"cmd\":\"pwd\"}"
        ));
    }
}
