use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::services::agent_protocol::{StreamMessage, is_valid_session_id};
use crate::services::process::kill_child_tree;
use crate::services::provider::{
    CancelToken, ProviderKind, StreamAttemptFailure, StreamAttemptResult, StreamFinalState,
    cancel_requested, register_child_pid, run_retrying_stream_attempts,
};
use crate::services::remote::RemoteProfile;

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";
const GEMINI_RESUME_LATEST: &str = "latest";
const GEMINI_SESSION_DEAD_MESSAGE: &str = "Gemini stream ended without a terminal result";
const GEMINI_INVALID_RESUME_SELECTOR_MESSAGE: &str =
    "InvalidArgument: Gemini resume selector must be `latest` or a numeric session index";
const GEMINI_STREAM_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const GEMINI_STREAM_IDLE_WATCHDOG: Duration = Duration::from_secs(120);
const GEMINI_STREAM_STARTUP_WATCHDOG: Duration = Duration::from_secs(60);
const GEMINI_MAX_SESSION_RETRIES: usize = 1;

#[derive(Debug)]
enum GeminiStreamEvent {
    Line(String),
    ReadError(String),
    Eof,
}

#[derive(Debug, PartialEq, Eq)]
enum GeminiStreamLoopResult {
    Eof,
    RetrySession { message: String },
    Cancelled,
}

#[derive(Debug, Default)]
struct GeminiAttemptState {
    final_text: String,
    raw_stdout: String,
    last_resume_selector: Option<String>,
    init_model: Option<String>,
    last_error_message: Option<String>,
    terminal_result_seen: bool,
    terminal_result_text: Option<String>,
    meaningful_progress_seen: bool,
}

impl GeminiAttemptState {
    fn new(last_resume_selector: Option<String>) -> Self {
        Self {
            last_resume_selector,
            ..Self::default()
        }
    }
}

pub fn resolve_gemini_path() -> Option<String> {
    crate::services::platform::resolve_provider_binary("gemini").resolved_path
}

fn resolve_gemini_binary() -> crate::services::platform::BinaryResolution {
    crate::services::platform::resolve_provider_binary("gemini")
}

fn get_gemini_path() -> Option<String> {
    resolve_gemini_path()
}

pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    let resolution = resolve_gemini_binary();
    let gemini_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Gemini CLI not found".to_string())?;
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut command = Command::new(&gemini_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    let output = command
        .args(build_exec_args(prompt, None, None))
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to start Gemini: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        return Err(derive_error_message(
            &stdout,
            &stderr,
            output.status.code(),
            "Gemini",
        ));
    }

    let text = extract_text_from_stream_output(&stdout);
    if text.trim().is_empty() {
        Err("Empty response from Gemini".to_string())
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
    _tmux_session_name: Option<&str>,
    _report_channel_id: Option<u64>,
    _report_provider: Option<ProviderKind>,
    model: Option<&str>,
    _compact_percent: Option<u64>,
) -> Result<(), String> {
    if remote_profile.is_some() {
        return Err(remote_profile_not_supported_message());
    }

    let resume_selector = normalize_resume_selector(session_id)?;
    if is_cancelled(cancel_token.as_deref()) {
        return Ok(());
    }

    let resolution = resolve_gemini_binary();
    let gemini_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Gemini CLI not found".to_string())?;
    let prompt = compose_gemini_prompt(prompt, system_prompt, allowed_tools);
    run_gemini_streaming_attempts(&sender, resume_selector, |resume_selector| {
        execute_gemini_streaming_attempt(
            &gemini_bin,
            &resolution,
            &prompt,
            model,
            resume_selector,
            working_dir,
            sender.clone(),
            cancel_token.clone(),
        )
    })
}

fn run_gemini_streaming_attempts<F>(
    sender: &Sender<StreamMessage>,
    resume_selector: Option<String>,
    mut execute_attempt: F,
) -> Result<(), String>
where
    F: FnMut(Option<String>) -> Result<StreamAttemptResult, String>,
{
    let mut attempt_index = 0usize;
    run_retrying_stream_attempts(
        "Gemini",
        resume_selector,
        GEMINI_MAX_SESSION_RETRIES,
        |resume_selector| {
            if attempt_index > 0 {
                let _ = sender.send(StreamMessage::RetryBoundary);
            }
            attempt_index += 1;
            execute_attempt(resume_selector)
        },
        |failure| send_gemini_stream_failure(sender, failure),
    )
}

fn execute_gemini_streaming_attempt(
    gemini_bin: &str,
    resolution: &crate::services::platform::BinaryResolution,
    prompt: &str,
    model: Option<&str>,
    resume_selector: Option<String>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
) -> Result<StreamAttemptResult, String> {
    let mut command = Command::new(gemini_bin);
    crate::services::platform::apply_binary_resolution(&mut command, resolution);
    let mut child = command
        .args(build_exec_args(prompt, model, resume_selector.as_deref()))
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Gemini: {}", e))?;

    register_child_pid(cancel_token.as_deref(), child.id());

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Gemini stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture Gemini stderr".to_string())?;
    let stdout_events = spawn_gemini_stream_reader(stdout);
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_string(&mut buf);
        buf
    });

    if is_cancelled(cancel_token.as_deref()) {
        kill_child_tree(&mut child);
        let _ = child.wait();
        let _ = stderr_handle.join();
        return Ok(StreamAttemptResult::Cancelled);
    }

    let mut state = GeminiAttemptState::new(resume_selector);
    match collect_gemini_stream_events(
        &stdout_events,
        &sender,
        cancel_token.as_deref(),
        &mut state,
        GEMINI_STREAM_POLL_TIMEOUT,
        GEMINI_STREAM_IDLE_WATCHDOG,
        GEMINI_STREAM_STARTUP_WATCHDOG,
        || {
            child
                .try_wait()
                .map(|status| status.is_some())
                .unwrap_or(true)
        },
    ) {
        GeminiStreamLoopResult::Cancelled => {
            kill_child_tree(&mut child);
            let _ = child.wait();
            let _ = stderr_handle.join();
            return Ok(StreamAttemptResult::Cancelled);
        }
        GeminiStreamLoopResult::RetrySession { message } => {
            kill_child_tree(&mut child);
            let _ = child.wait();
            let stderr = stderr_handle.join().unwrap_or_default();
            return Ok(StreamAttemptResult::RetrySession(StreamAttemptFailure {
                message,
                stdout: state.raw_stdout,
                stderr,
                exit_code: None,
            }));
        }
        GeminiStreamLoopResult::Eof => {}
    }

    let status = child
        .wait()
        .map_err(|e| format!("Failed waiting for Gemini: {}", e))?;
    let stderr = stderr_handle.join().unwrap_or_default();

    if is_cancelled(cancel_token.as_deref()) {
        return Ok(StreamAttemptResult::Cancelled);
    }

    match finalize_gemini_attempt(&mut state, stderr, status.code()) {
        StreamFinalState::Done { result, session_id } => {
            let _ = sender.send(StreamMessage::Done { result, session_id });
            Ok(StreamAttemptResult::Completed)
        }
        StreamFinalState::Error(failure) => {
            send_gemini_stream_failure(&sender, failure);
            Ok(StreamAttemptResult::Completed)
        }
        StreamFinalState::RetrySession(failure) => Ok(StreamAttemptResult::RetrySession(failure)),
    }
}

fn send_gemini_stream_failure(sender: &Sender<StreamMessage>, failure: StreamAttemptFailure) {
    let StreamAttemptFailure {
        message,
        stdout,
        stderr,
        exit_code,
    } = failure;
    let _ = sender.send(StreamMessage::Error {
        message,
        stdout,
        stderr,
        exit_code,
    });
}

fn collect_gemini_stream_events<F>(
    stdout_events: &mpsc::Receiver<GeminiStreamEvent>,
    sender: &Sender<StreamMessage>,
    cancel_token: Option<&CancelToken>,
    state: &mut GeminiAttemptState,
    poll_timeout: Duration,
    idle_watchdog: Duration,
    startup_watchdog: Duration,
    mut definitive_failure_observed: F,
) -> GeminiStreamLoopResult
where
    F: FnMut() -> bool,
{
    let mut silent_for = Duration::ZERO;
    let mut startup_silent_for = Duration::ZERO;

    loop {
        if is_cancelled(cancel_token) {
            return GeminiStreamLoopResult::Cancelled;
        }

        match stdout_events.recv_timeout(poll_timeout) {
            Ok(GeminiStreamEvent::Line(line)) => {
                silent_for = Duration::ZERO;
                startup_silent_for = Duration::ZERO;
                process_gemini_stream_line(&line, state, sender);
            }
            Ok(GeminiStreamEvent::ReadError(message)) => {
                return GeminiStreamLoopResult::RetrySession { message };
            }
            Ok(GeminiStreamEvent::Eof) | Err(RecvTimeoutError::Disconnected) => {
                return GeminiStreamLoopResult::Eof;
            }
            Err(RecvTimeoutError::Timeout) => {
                if is_cancelled(cancel_token) {
                    return GeminiStreamLoopResult::Cancelled;
                }
                if state.terminal_result_seen {
                    return GeminiStreamLoopResult::Eof;
                }
                if !state.meaningful_progress_seen {
                    startup_silent_for += poll_timeout;
                    if startup_silent_for >= startup_watchdog {
                        let _ = definitive_failure_observed();
                        return GeminiStreamLoopResult::RetrySession {
                            message: format!(
                                "Gemini stream produced no output for {} seconds before first progress",
                                startup_watchdog.as_secs()
                            ),
                        };
                    }
                    continue;
                }
                silent_for += poll_timeout;
                if silent_for >= idle_watchdog {
                    let _ = definitive_failure_observed();
                    return GeminiStreamLoopResult::RetrySession {
                        message: format!(
                            "Gemini stream produced no output for {} seconds",
                            idle_watchdog.as_secs()
                        ),
                    };
                }
            }
        }
    }
}

fn spawn_gemini_stream_reader<R>(stdout: R) -> mpsc::Receiver<GeminiStreamEvent>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if tx.send(GeminiStreamEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(GeminiStreamEvent::ReadError(format!(
                        "Failed reading Gemini output: {}",
                        e
                    )));
                    return;
                }
            }
        }
        let _ = tx.send(GeminiStreamEvent::Eof);
    });
    rx
}

fn process_gemini_stream_line(
    line: &str,
    state: &mut GeminiAttemptState,
    sender: &Sender<StreamMessage>,
) {
    if line.trim().is_empty() {
        return;
    }
    state.raw_stdout.push_str(line);
    state.raw_stdout.push('\n');

    let Ok(json) = serde_json::from_str::<Value>(line.trim()) else {
        return;
    };

    process_gemini_json_event(&json, state, sender);
}

fn process_gemini_json_event(
    json: &Value,
    state: &mut GeminiAttemptState,
    sender: &Sender<StreamMessage>,
) {
    match json.get("type").and_then(|v| v.as_str()) {
        Some("init") => {
            if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
                // P1: Preserve existing numeric/latest resume selector — only update
                // if the observed value is itself resumable, or we have no selector yet.
                let observed = observed_session_to_resume_selector(session_id);
                let existing_is_resumable =
                    state.last_resume_selector.as_ref().map_or(false, |s| {
                        s == GEMINI_RESUME_LATEST || s.chars().all(|c| c.is_ascii_digit())
                    });
                if !existing_is_resumable
                    || observed
                        .as_ref()
                        .map_or(false, |o| o != GEMINI_RESUME_LATEST)
                {
                    state.last_resume_selector = observed;
                }
                let _ = sender.send(StreamMessage::Init {
                    session_id: state
                        .last_resume_selector
                        .clone()
                        .unwrap_or_else(|| GEMINI_RESUME_LATEST.to_string()),
                });
            }
            state.init_model = json
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
        Some("message") => {
            let role = json.get("role").and_then(|v| v.as_str());
            let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if role == Some("assistant") && !content.is_empty() {
                state.meaningful_progress_seen = true;
                state.final_text.push_str(content);
                let _ = sender.send(StreamMessage::Text {
                    content: content.to_string(),
                });
            }
        }
        Some("tool_use") => {
            if let Some(tool_use) = build_gemini_tool_use_message(json) {
                state.meaningful_progress_seen = true;
                let _ = sender.send(tool_use);
            }
        }
        Some("tool_result") => {
            if let Some(tool_result) = build_gemini_tool_result_message(json) {
                state.meaningful_progress_seen = true;
                let _ = sender.send(tool_result);
            }
        }
        Some("error") => {
            state.last_error_message = extract_gemini_error_message(json);
        }
        Some("result") => {
            state.terminal_result_seen = true;
            state.terminal_result_text = json
                .get("result")
                .map(render_gemini_value)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            let stats = json.get("stats");
            let model_name = state.init_model.clone().or_else(|| {
                stats
                    .and_then(|value| value.get("models"))
                    .and_then(|value| value.as_object())
                    .and_then(|models| models.keys().next().cloned())
            });
            let input_tokens = stats
                .and_then(|value| value.get("input_tokens"))
                .and_then(|value| value.as_u64())
                .or_else(|| {
                    stats
                        .and_then(|value| value.get("input"))
                        .and_then(|value| value.as_u64())
                });
            let output_tokens = stats
                .and_then(|value| value.get("output_tokens"))
                .and_then(|value| value.as_u64());
            let duration_ms = stats
                .and_then(|value| value.get("duration_ms"))
                .and_then(|value| value.as_u64());
            let _ = sender.send(StreamMessage::StatusUpdate {
                model: model_name,
                cost_usd: None,
                total_cost_usd: None,
                duration_ms,
                num_turns: None,
                input_tokens,
                output_tokens,
            });
        }
        _ => {}
    }
}

fn finalize_gemini_attempt(
    state: &mut GeminiAttemptState,
    stderr: String,
    exit_code: Option<i32>,
) -> StreamFinalState {
    let final_text = std::mem::take(&mut state.final_text);
    let raw_stdout = std::mem::take(&mut state.raw_stdout);
    let last_resume_selector = state.last_resume_selector.take();
    let last_error_message = state.last_error_message.take();
    let terminal_result_seen = state.terminal_result_seen;
    let terminal_result_text = state.terminal_result_text.take();

    if let Some(message) = last_error_message {
        return StreamFinalState::Error(StreamAttemptFailure {
            message,
            stdout: raw_stdout,
            stderr,
            exit_code,
        });
    }

    if exit_code.unwrap_or(0) != 0 {
        return StreamFinalState::Error(StreamAttemptFailure {
            message: derive_error_message(&raw_stdout, &stderr, exit_code, "Gemini"),
            stdout: raw_stdout,
            stderr,
            exit_code,
        });
    }

    if terminal_result_seen {
        let result = final_text.trim().to_string();
        let result = if result.is_empty() {
            terminal_result_text.unwrap_or_default()
        } else {
            result
        };
        if result.is_empty() {
            return StreamFinalState::Error(StreamAttemptFailure {
                message: "Gemini emitted a terminal result without any response text".to_string(),
                stdout: raw_stdout,
                stderr,
                exit_code,
            });
        }
        return StreamFinalState::Done {
            result,
            session_id: Some(
                last_resume_selector.unwrap_or_else(|| GEMINI_RESUME_LATEST.to_string()),
            ),
        };
    }

    StreamFinalState::RetrySession(StreamAttemptFailure {
        message: GEMINI_SESSION_DEAD_MESSAGE.to_string(),
        stdout: raw_stdout,
        stderr,
        exit_code,
    })
}

fn is_cancelled(token: Option<&CancelToken>) -> bool {
    cancel_requested(token)
}

fn remote_profile_not_supported_message() -> String {
    "NotSupported: Gemini provider does not support remote execution yet. Remove `remote_profile` or use a provider with remote support.".to_string()
}

fn compose_gemini_prompt(
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> String {
    crate::services::provider::compose_structured_turn_prompt(prompt, system_prompt, allowed_tools)
}

fn build_exec_args(prompt: &str, model: Option<&str>, session_id: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    let session_id = session_id.map(str::trim).filter(|value| !value.is_empty());
    if session_id.is_none() {
        let model = model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_GEMINI_MODEL);
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    if let Some(session_id) = session_id {
        args.push("--resume".to_string());
        args.push(session_id.to_string());
    }
    args.push("-p".to_string());
    args.push(prompt.to_string());
    args.push("--output-format".to_string());
    args.push("stream-json".to_string());
    args.push("-y".to_string());
    args.push("--sandbox".to_string());
    args.push("false".to_string());
    args
}

fn normalize_resume_selector(session_id: Option<&str>) -> Result<Option<String>, String> {
    let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    if session_id.eq_ignore_ascii_case(GEMINI_RESUME_LATEST) {
        return Ok(Some(GEMINI_RESUME_LATEST.to_string()));
    }

    if session_id.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(Some(session_id.to_string()));
    }

    if looks_like_uuid(session_id) {
        // Gemini 0.35.3 exposes UUID-like session metadata in `init`, but `--resume`
        // accepts `latest` or a numeric index. Normalize persisted legacy values.
        return Ok(Some(GEMINI_RESUME_LATEST.to_string()));
    }

    if is_common_session_metadata(session_id) {
        return Err(GEMINI_INVALID_RESUME_SELECTOR_MESSAGE.to_string());
    }

    Err(GEMINI_INVALID_RESUME_SELECTOR_MESSAGE.to_string())
}

fn observed_session_to_resume_selector(session_id: &str) -> Option<String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }

    if session_id.eq_ignore_ascii_case(GEMINI_RESUME_LATEST) {
        return Some(GEMINI_RESUME_LATEST.to_string());
    }

    if session_id.chars().all(|ch| ch.is_ascii_digit()) {
        return Some(session_id.to_string());
    }

    if looks_like_uuid(session_id) || is_common_session_metadata(session_id) {
        return Some(GEMINI_RESUME_LATEST.to_string());
    }

    None
}

fn is_common_session_metadata(session_id: &str) -> bool {
    let session_id = session_id.trim();
    !session_id.is_empty() && is_valid_session_id(session_id)
}

fn looks_like_uuid(value: &str) -> bool {
    let mut parts = value.split('-');
    let expected = [8, 4, 4, 4, 12];
    for len in expected {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != len || !part.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

fn extract_text_from_stream_output(output: &str) -> String {
    let mut final_text = String::new();
    for line in output.lines() {
        let Ok(json) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let is_assistant = json.get("type").and_then(|v| v.as_str()) == Some("message")
            && json.get("role").and_then(|v| v.as_str()) == Some("assistant");
        if !is_assistant {
            continue;
        }
        let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if !content.is_empty() {
            final_text.push_str(content);
        }
    }
    final_text.trim().to_string()
}

fn derive_error_message(stdout: &str, stderr: &str, exit_code: Option<i32>, label: &str) -> String {
    if !stderr.trim().is_empty() {
        return stderr.trim().to_string();
    }

    if let Some(last) = stdout.lines().rev().find(|line| !line.trim().is_empty()) {
        let last = last.trim();
        if !last.is_empty() {
            return last.to_string();
        }
    }

    format!("{} exited with code {:?}", label, exit_code)
}

fn build_gemini_tool_use_message(json: &Value) -> Option<StreamMessage> {
    let tool_name = json.get("tool_name")?.as_str()?.trim();
    if tool_name.is_empty() {
        return None;
    }

    let mapped_name = map_gemini_tool_name(tool_name).to_string();
    let input = json
        .get("parameters")
        .map(render_gemini_value)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "{}".to_string());

    Some(StreamMessage::ToolUse {
        name: mapped_name,
        input,
    })
}

fn build_gemini_tool_result_message(json: &Value) -> Option<StreamMessage> {
    let status = json
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("success");
    let content = json
        .get("output")
        .map(render_gemini_value)
        .or_else(|| json.get("error").map(render_gemini_value))
        .or_else(|| json.get("result").map(render_gemini_value))
        .unwrap_or_default();

    Some(StreamMessage::ToolResult {
        content,
        is_error: status != "success",
    })
}

fn extract_gemini_error_message(json: &Value) -> Option<String> {
    json.get("message")
        .or_else(|| json.get("error"))
        .map(render_gemini_value)
        .or_else(|| {
            json.get("details")
                .and_then(|details| details.as_array())
                .and_then(|details| details.first())
                .map(render_gemini_value)
        })
        .map(|message| message.trim().to_string())
        .filter(|message| !message.is_empty())
}

fn map_gemini_tool_name(tool_name: &str) -> &str {
    match tool_name {
        "run_shell_command" => "Bash",
        "read_many_files" | "read_file" => "Read",
        "write_file" => "Write",
        "replace" | "edit_file" => "Edit",
        "glob" => "Glob",
        "grep" => "Grep",
        "web_search" => "WebSearch",
        "web_fetch" => "WebFetch",
        other => other,
    }
}

fn render_gemini_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.to_string(),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GEMINI_INVALID_RESUME_SELECTOR_MESSAGE, GEMINI_SESSION_DEAD_MESSAGE, GeminiAttemptState,
        GeminiStreamEvent, GeminiStreamLoopResult, build_exec_args,
        build_gemini_tool_result_message, build_gemini_tool_use_message,
        collect_gemini_stream_events, execute_command_streaming, extract_gemini_error_message,
        extract_text_from_stream_output, finalize_gemini_attempt, looks_like_uuid,
        normalize_resume_selector, observed_session_to_resume_selector, process_gemini_stream_line,
        remote_profile_not_supported_message, run_gemini_streaming_attempts,
    };
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::provider::{
        CancelToken, StreamAttemptFailure, StreamAttemptResult, StreamFinalState,
    };
    use crate::services::remote::{RemoteAuth, RemoteProfile};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn build_exec_args_includes_resume_when_session_present() {
        let args = build_exec_args("hello", Some("gemini-2.5-flash"), Some("latest"));
        assert!(args.windows(2).any(|pair| pair == ["--resume", "latest"]));
        assert!(args.windows(2).any(|pair| pair == ["-p", "hello"]));
        assert!(
            !args
                .windows(2)
                .any(|pair| pair == ["-m", "gemini-2.5-flash"])
        );
    }

    #[test]
    fn build_exec_args_includes_model_for_fresh_session() {
        let args = build_exec_args("hello", Some("gemini-2.5-flash"), None);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-m", "gemini-2.5-flash"])
        );
        assert!(!args.iter().any(|arg| arg == "--resume"));
    }

    #[test]
    fn normalize_resume_selector_accepts_latest_and_numeric_index() {
        assert_eq!(
            normalize_resume_selector(Some("latest"))
                .unwrap()
                .as_deref(),
            Some("latest")
        );
        assert_eq!(
            normalize_resume_selector(Some("12")).unwrap().as_deref(),
            Some("12")
        );
    }

    #[test]
    fn normalize_resume_selector_maps_uuid_like_metadata_to_latest() {
        let observed = "aa678e6b-c6d3-4dd2-9197-58580c00cc6c";
        assert!(looks_like_uuid(observed));
        assert_eq!(
            normalize_resume_selector(Some(observed))
                .unwrap()
                .as_deref(),
            Some("latest")
        );
        assert_eq!(
            observed_session_to_resume_selector(observed).as_deref(),
            Some("latest")
        );
    }

    #[test]
    fn observed_session_to_resume_selector_preserves_numeric_selector() {
        assert_eq!(
            observed_session_to_resume_selector("12").as_deref(),
            Some("12")
        );
    }

    #[test]
    fn normalize_resume_selector_rejects_arbitrary_strings() {
        let error = normalize_resume_selector(Some("session-alpha")).unwrap_err();
        assert!(error.contains("InvalidArgument"));
    }

    #[test]
    fn observed_session_to_resume_selector_maps_common_metadata_to_latest() {
        assert_eq!(
            observed_session_to_resume_selector("session-alpha").as_deref(),
            Some("latest")
        );
    }

    #[test]
    fn extract_text_from_stream_output_ignores_plaintext_retry_logs() {
        let output = concat!(
            "Attempt 1 failed with status 429. Retrying with backoff...\n",
            "{\"type\":\"init\",\"session_id\":\"aa678e6b-c6d3-4dd2-9197-58580c00cc6c\"}\n",
            "{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"OK\"}\n"
        );
        assert_eq!(extract_text_from_stream_output(output), "OK");
    }

    #[test]
    fn tool_use_event_maps_shell_command_to_bash() {
        let event = json!({
            "type": "tool_use",
            "tool_name": "run_shell_command",
            "parameters": {
                "description": "Print working directory",
                "command": "pwd"
            }
        });

        match build_gemini_tool_use_message(&event) {
            Some(StreamMessage::ToolUse { name, input }) => {
                assert_eq!(name, "Bash");
                assert!(input.contains("\"command\":\"pwd\""));
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn tool_result_event_maps_output_and_error_flag() {
        let event = json!({
            "type": "tool_result",
            "status": "success",
            "output": "/tmp/example"
        });

        match build_gemini_tool_result_message(&event) {
            Some(StreamMessage::ToolResult { content, is_error }) => {
                assert_eq!(content, "/tmp/example");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn tool_use_then_tool_result_preserves_order() {
        let (tx, rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        process_gemini_stream_line(
            r#"{"type":"tool_use","tool_name":"run_shell_command","parameters":{"command":"pwd"}}"#,
            &mut state,
            &tx,
        );
        process_gemini_stream_line(
            r#"{"type":"tool_result","status":"success","output":"/tmp/example"}"#,
            &mut state,
            &tx,
        );

        match rx.recv().unwrap() {
            StreamMessage::ToolUse { name, input } => {
                assert_eq!(name, "Bash");
                assert!(input.contains("\"command\":\"pwd\""));
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
        match rx.recv().unwrap() {
            StreamMessage::ToolResult { content, is_error } => {
                assert_eq!(content, "/tmp/example");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn error_event_prefers_message_field() {
        let event = json!({
            "type": "error",
            "message": "quota exceeded"
        });

        assert_eq!(
            extract_gemini_error_message(&event).as_deref(),
            Some("quota exceeded")
        );
    }

    #[test]
    fn parser_schema_drift_is_ignored_without_panicking() {
        let (_tx, rx): (mpsc::Sender<StreamMessage>, mpsc::Receiver<StreamMessage>) =
            mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        process_gemini_stream_line(
            r#"{"type":"message","role":42,"content":["bad-shape"]}"#,
            &mut state,
            &_tx,
        );

        assert!(state.final_text.is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn parser_empty_payload_is_ignored() {
        let (_tx, rx): (mpsc::Sender<StreamMessage>, mpsc::Receiver<StreamMessage>) =
            mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        process_gemini_stream_line("{}", &mut state, &_tx);

        assert!(!state.terminal_result_seen);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn attempt_messages_are_emitted_immediately() {
        let (tx, rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        process_gemini_stream_line(
            r#"{"type":"init","session_id":"aa678e6b-c6d3-4dd2-9197-58580c00cc6c","model":"gemini-2.5-flash"}"#,
            &mut state,
            &tx,
        );
        process_gemini_stream_line(
            r#"{"type":"message","role":"assistant","content":"hello"}"#,
            &mut state,
            &tx,
        );

        match rx.recv().unwrap() {
            StreamMessage::Init { session_id } => assert_eq!(session_id, "latest"),
            other => panic!("expected Init, got {:?}", other),
        }
        match rx.recv().unwrap() {
            StreamMessage::Text { content } => assert_eq!(content, "hello"),
            other => panic!("expected Text, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn execute_complete_emits_stream_events_before_done() {
        let (tx, rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        process_gemini_stream_line(
            r#"{"type":"init","session_id":"session-alpha","model":"gemini-2.5-flash"}"#,
            &mut state,
            &tx,
        );
        process_gemini_stream_line(
            r#"{"type":"message","role":"assistant","content":"hello"}"#,
            &mut state,
            &tx,
        );
        process_gemini_stream_line(
            r#"{"type":"tool_use","tool_name":"run_shell_command","parameters":{"command":"pwd"}}"#,
            &mut state,
            &tx,
        );
        process_gemini_stream_line(
            r#"{"type":"tool_result","status":"success","output":"/tmp/example"}"#,
            &mut state,
            &tx,
        );
        process_gemini_stream_line(
            r#"{"type":"result","result":"hello","stats":{"input_tokens":10,"output_tokens":4,"duration_ms":20}}"#,
            &mut state,
            &tx,
        );

        let final_state = finalize_gemini_attempt(&mut state, String::new(), Some(0));
        match final_state {
            StreamFinalState::Done { result, session_id } => {
                let _ = tx.send(StreamMessage::Done { result, session_id });
            }
            other => panic!("expected Done, got {:?}", other),
        }

        match rx.recv().unwrap() {
            StreamMessage::Init { session_id } => assert_eq!(session_id, "latest"),
            other => panic!("expected Init, got {:?}", other),
        }
        match rx.recv().unwrap() {
            StreamMessage::Text { content } => assert_eq!(content, "hello"),
            other => panic!("expected Text, got {:?}", other),
        }
        match rx.recv().unwrap() {
            StreamMessage::ToolUse { name, .. } => assert_eq!(name, "Bash"),
            other => panic!("expected ToolUse, got {:?}", other),
        }
        match rx.recv().unwrap() {
            StreamMessage::ToolResult { content, is_error } => {
                assert_eq!(content, "/tmp/example");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
        match rx.recv().unwrap() {
            StreamMessage::StatusUpdate {
                model,
                input_tokens,
                output_tokens,
                duration_ms,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("gemini-2.5-flash"));
                assert_eq!(input_tokens, Some(10));
                assert_eq!(output_tokens, Some(4));
                assert_eq!(duration_ms, Some(20));
            }
            other => panic!("expected StatusUpdate, got {:?}", other),
        }
        match rx.recv().unwrap() {
            StreamMessage::Done { result, session_id } => {
                assert_eq!(result, "hello");
                assert_eq!(session_id.as_deref(), Some("latest"));
            }
            other => panic!("expected Done, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn terminal_result_with_structured_error_is_terminal_error() {
        let mut state = GeminiAttemptState::new(Some("latest".to_string()));
        state.last_error_message = Some("quota exceeded".to_string());
        state.final_text = "done".to_string();
        state.terminal_result_seen = true;

        match finalize_gemini_attempt(&mut state, String::new(), Some(0)) {
            StreamFinalState::Error(failure) => {
                assert_eq!(failure.message, "quota exceeded");
                assert!(failure.stdout.is_empty());
                assert!(failure.stderr.is_empty());
                assert_eq!(failure.exit_code, Some(0));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn assistant_text_without_terminal_result_requests_retry() {
        let mut state = GeminiAttemptState::new(None);
        state.final_text = "partial text".to_string();
        state.raw_stdout =
            "{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"partial text\"}\n"
                .to_string();

        match finalize_gemini_attempt(&mut state, String::new(), Some(0)) {
            StreamFinalState::RetrySession(failure) => {
                assert_eq!(failure.message, GEMINI_SESSION_DEAD_MESSAGE);
            }
            other => panic!("expected RetrySession, got {:?}", other),
        }
    }

    #[test]
    fn non_zero_exit_without_structured_error_is_terminal_error() {
        let mut state = GeminiAttemptState::new(Some("latest".to_string()));
        state.raw_stdout = "plain stdout".to_string();

        match finalize_gemini_attempt(&mut state, "plain stderr".to_string(), Some(2)) {
            StreamFinalState::Error(failure) => {
                assert!(failure.message.contains("plain stderr"));
                assert_eq!(failure.stdout, "plain stdout");
                assert_eq!(failure.stderr, "plain stderr");
                assert_eq!(failure.exit_code, Some(2));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn terminal_result_with_non_zero_exit_is_terminal_error() {
        let mut state = GeminiAttemptState::new(Some("latest".to_string()));
        state.final_text = "done".to_string();
        state.terminal_result_seen = true;
        state.raw_stdout = "plain stdout".to_string();

        match finalize_gemini_attempt(&mut state, "plain stderr".to_string(), Some(2)) {
            StreamFinalState::Error(failure) => {
                assert!(failure.message.contains("plain stderr"));
                assert_eq!(failure.stdout, "plain stdout");
                assert_eq!(failure.stderr, "plain stderr");
                assert_eq!(failure.exit_code, Some(2));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn terminal_result_without_init_falls_back_to_latest_selector() {
        let mut state = GeminiAttemptState::new(None);
        state.final_text = "done".to_string();
        state.terminal_result_seen = true;

        match finalize_gemini_attempt(&mut state, String::new(), Some(0)) {
            StreamFinalState::Done { result, session_id } => {
                assert_eq!(result, "done");
                assert_eq!(session_id.as_deref(), Some("latest"));
            }
            other => panic!("expected Done, got {:?}", other),
        }
    }

    #[test]
    fn terminal_result_with_zero_exit_ignores_stderr_noise() {
        let mut state = GeminiAttemptState::new(Some("latest".to_string()));
        state.final_text = "done".to_string();
        state.terminal_result_seen = true;
        state.raw_stdout = "plain stdout".to_string();

        match finalize_gemini_attempt(&mut state, "warning: progress".to_string(), Some(0)) {
            StreamFinalState::Done { result, session_id } => {
                assert_eq!(result, "done");
                assert_eq!(session_id.as_deref(), Some("latest"));
            }
            other => panic!("expected Done, got {:?}", other),
        }
    }

    #[test]
    fn remote_profile_not_supported_message_has_guidance() {
        let message = remote_profile_not_supported_message();
        assert!(message.contains("NotSupported"));
        assert!(message.contains("remote_profile"));
    }

    #[test]
    fn execute_command_streaming_rejects_invalid_resume_selector_before_binary_lookup() {
        let (tx, _rx) = mpsc::channel();
        let error = execute_command_streaming(
            "hello",
            Some("session-alpha"),
            ".",
            tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();

        assert_eq!(error, GEMINI_INVALID_RESUME_SELECTOR_MESSAGE);
    }

    #[test]
    fn execute_command_streaming_returns_ok_when_cancelled_before_spawn() {
        let (tx, rx) = mpsc::channel();
        let token = Arc::new(CancelToken::new());
        token.cancelled.store(true, Ordering::Relaxed);

        let result = execute_command_streaming(
            "hello",
            None,
            ".",
            tx,
            None,
            None,
            Some(token.clone()),
            None,
            None,
            None,
            None,
            None,
            None,
        );

        assert!(result.is_ok());
        assert!(rx.try_recv().is_err());
        assert!(token.child_pid.lock().unwrap().is_none());
    }

    #[test]
    fn cancelled_during_stream_returns_cancelled() {
        let token = Arc::new(CancelToken::new());
        let (tx, rx) = mpsc::channel();
        let (stream_tx, _stream_rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        tx.send(GeminiStreamEvent::Line(
            r#"{"type":"message","role":"assistant","content":"partial"}"#.to_string(),
        ))
        .unwrap();
        let token_for_thread = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1));
            token_for_thread.cancelled.store(true, Ordering::Relaxed);
        });

        let result = collect_gemini_stream_events(
            &rx,
            &stream_tx,
            Some(token.as_ref()),
            &mut state,
            Duration::from_millis(5),
            Duration::from_millis(10),
            Duration::from_millis(100),
            || false,
        );

        assert_eq!(result, GeminiStreamLoopResult::Cancelled);
        assert_eq!(state.final_text, "partial");
    }

    #[test]
    fn idle_watchdog_does_not_retry_before_first_stream_progress() {
        let token = Arc::new(CancelToken::new());
        let (tx, rx) = mpsc::channel();
        let (stream_tx, stream_rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        tx.send(GeminiStreamEvent::Line(
            r#"{"type":"init","session_id":"latest","model":"gemini-2.5-flash"}"#.to_string(),
        ))
        .unwrap();
        let token_for_thread = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(4));
            token_for_thread.cancelled.store(true, Ordering::Relaxed);
        });

        let result = collect_gemini_stream_events(
            &rx,
            &stream_tx,
            Some(token.as_ref()),
            &mut state,
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(100), // startup_watchdog >> cancel delay → won't fire
            || false,
        );

        assert_eq!(result, GeminiStreamLoopResult::Cancelled);
        assert!(!state.raw_stdout.is_empty());
        assert!(!state.meaningful_progress_seen);
        match stream_rx.recv().unwrap() {
            StreamMessage::Init { session_id } => assert_eq!(session_id, "latest"),
            other => panic!("expected Init, got {:?}", other),
        }
        assert!(stream_rx.try_recv().is_err());
    }

    #[test]
    fn idle_watchdog_retries_if_process_is_still_alive_during_extended_silence() {
        let (tx, rx) = mpsc::channel();
        let (stream_tx, stream_rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        tx.send(GeminiStreamEvent::Line(
            r#"{"type":"message","role":"assistant","content":"partial"}"#.to_string(),
        ))
        .unwrap();

        let result = collect_gemini_stream_events(
            &rx,
            &stream_tx,
            None,
            &mut state,
            Duration::from_millis(1),
            Duration::from_millis(3),
            Duration::from_millis(100),
            || false,
        );

        match result {
            GeminiStreamLoopResult::RetrySession { message } => {
                assert!(message.contains("Gemini stream produced no output"));
            }
            other => panic!("expected RetrySession, got {:?}", other),
        }
        assert_eq!(state.final_text, "partial");
        assert!(state.meaningful_progress_seen);
        match stream_rx.recv().unwrap() {
            StreamMessage::Text { content } => assert_eq!(content, "partial"),
            other => panic!("expected Text, got {:?}", other),
        }
        assert!(stream_rx.try_recv().is_err());
    }

    #[test]
    fn idle_watchdog_retries_after_extended_silence_once_process_exit_is_observed() {
        let (tx, rx) = mpsc::channel();
        let (stream_tx, stream_rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        tx.send(GeminiStreamEvent::Line(
            r#"{"type":"message","role":"assistant","content":"partial"}"#.to_string(),
        ))
        .unwrap();

        let result = collect_gemini_stream_events(
            &rx,
            &stream_tx,
            None,
            &mut state,
            Duration::from_millis(1),
            Duration::from_millis(3),
            Duration::from_millis(100),
            || true,
        );

        match result {
            GeminiStreamLoopResult::RetrySession { message } => {
                assert!(message.contains("Gemini stream produced no output"));
            }
            other => panic!("expected RetrySession, got {:?}", other),
        }
        assert_eq!(state.final_text, "partial");
        assert!(state.meaningful_progress_seen);
        match stream_rx.recv().unwrap() {
            StreamMessage::Text { content } => assert_eq!(content, "partial"),
            other => panic!("expected Text, got {:?}", other),
        }
        assert!(stream_rx.try_recv().is_err());
    }

    #[test]
    fn session_died_retry_once_then_error() {
        let (tx, rx) = mpsc::channel();
        let mut attempt_calls = Vec::new();

        let result = run_gemini_streaming_attempts(&tx, Some("latest".to_string()), |selector| {
            attempt_calls.push(selector);
            Ok(StreamAttemptResult::RetrySession(StreamAttemptFailure {
                message: GEMINI_SESSION_DEAD_MESSAGE.to_string(),
                stdout: "partial".to_string(),
                stderr: String::new(),
                exit_code: None,
            }))
        });

        assert!(result.is_ok());
        assert_eq!(attempt_calls, vec![Some("latest".to_string()), None]);
        match rx.recv().unwrap() {
            StreamMessage::RetryBoundary => {}
            other => panic!("expected RetryBoundary, got {:?}", other),
        }
        match rx.recv().unwrap() {
            StreamMessage::Error {
                message,
                stdout,
                stderr,
                exit_code,
            } => {
                assert!(message.contains("could not be recovered after retry"));
                assert!(message.contains(GEMINI_SESSION_DEAD_MESSAGE));
                assert_eq!(stdout, "partial");
                assert!(stderr.is_empty());
                assert_eq!(exit_code, None);
            }
            other => panic!("expected Error, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn retry_success_without_init_emits_boundary_and_latest_selector() {
        let (tx, rx) = mpsc::channel();
        let mut attempt_calls = Vec::new();

        let result = run_gemini_streaming_attempts(&tx, Some("latest".to_string()), |selector| {
            attempt_calls.push(selector.clone());
            if attempt_calls.len() == 1 {
                return Ok(StreamAttemptResult::RetrySession(StreamAttemptFailure {
                    message: GEMINI_SESSION_DEAD_MESSAGE.to_string(),
                    stdout: "partial".to_string(),
                    stderr: String::new(),
                    exit_code: None,
                }));
            }

            let mut state = GeminiAttemptState::new(selector);
            state.final_text = "fresh result".to_string();
            state.terminal_result_seen = true;
            match finalize_gemini_attempt(&mut state, String::new(), Some(0)) {
                StreamFinalState::Done { result, session_id } => {
                    let _ = tx.send(StreamMessage::Done { result, session_id });
                    Ok(StreamAttemptResult::Completed)
                }
                other => panic!("expected Done, got {:?}", other),
            }
        });

        assert!(result.is_ok());
        assert_eq!(attempt_calls, vec![Some("latest".to_string()), None]);
        match rx.recv().unwrap() {
            StreamMessage::RetryBoundary => {}
            other => panic!("expected RetryBoundary, got {:?}", other),
        }
        match rx.recv().unwrap() {
            StreamMessage::Done { result, session_id } => {
                assert_eq!(result, "fresh result");
                assert_eq!(session_id.as_deref(), Some("latest"));
            }
            other => panic!("expected Done, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn startup_watchdog_fires_retry_before_first_progress() {
        // Before any meaningful output, silence >= startup_watchdog with process exited
        // must trigger RetrySession.
        let (_tx, rx) = mpsc::channel::<GeminiStreamEvent>(); // keep alive so Timeout fires
        let (stream_tx, _stream_rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);

        let result = collect_gemini_stream_events(
            &rx,
            &stream_tx,
            None,
            &mut state,
            Duration::from_millis(1),   // poll_timeout
            Duration::from_millis(100), // idle_watchdog (never reached)
            Duration::from_millis(3),   // startup_watchdog: short for test
            || true,                    // process has exited
        );

        match result {
            GeminiStreamLoopResult::RetrySession { message } => {
                assert!(
                    message.contains("before first progress"),
                    "expected startup watchdog message, got: {message}"
                );
            }
            other => panic!(
                "expected RetrySession from startup watchdog, got {:?}",
                other
            ),
        }
        assert!(!state.meaningful_progress_seen);
    }

    #[test]
    fn startup_watchdog_fires_if_process_is_still_alive() {
        let (_tx, rx) = mpsc::channel::<GeminiStreamEvent>(); // keep alive
        let (stream_tx, _stream_rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);

        let result = collect_gemini_stream_events(
            &rx,
            &stream_tx,
            None,
            &mut state,
            Duration::from_millis(1),
            Duration::from_millis(100),
            Duration::from_millis(3),
            || false,
        );

        match result {
            GeminiStreamLoopResult::RetrySession { message } => {
                assert!(
                    message.contains("before first progress"),
                    "expected startup watchdog message, got: {message}"
                );
            }
            other => panic!("expected RetrySession, got {:?}", other),
        }
    }

    #[test]
    fn execute_command_streaming_rejects_remote_profile_before_spawn() {
        let (tx, _rx) = mpsc::channel();
        let remote_profile = RemoteProfile {
            name: "test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            user: "kunkun".to_string(),
            auth: RemoteAuth::Password {
                password: "secret".to_string(),
            },
            default_path: "/tmp".to_string(),
            claude_path: None,
        };

        let error = execute_command_streaming(
            "hello",
            None,
            ".",
            tx,
            None,
            None,
            None,
            Some(&remote_profile),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();

        assert!(error.contains("NotSupported"));
    }
}
