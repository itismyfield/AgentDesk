use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::services::claude::{self, CancelToken, StreamMessage};
use crate::services::provider::ProviderKind;
use crate::services::remote::RemoteProfile;

static GEMINI_PATH: OnceLock<Option<String>> = OnceLock::new();
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";
const GEMINI_RESUME_LATEST: &str = "latest";
const GEMINI_CANCELLED_MESSAGE: &str = "Gemini request cancelled";
const GEMINI_SESSION_DEAD_MESSAGE: &str = "Gemini stream ended without a terminal result";
const GEMINI_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const GEMINI_STREAM_IDLE_TICKS_BEFORE_RETRY: u32 = 2;
const GEMINI_MAX_SESSION_RETRIES: usize = 1;

#[derive(Debug)]
enum GeminiStreamEvent {
    Line(String),
    ReadError(String),
    Eof,
}

#[derive(Debug)]
enum GeminiAttemptResult {
    Completed,
    RetrySession {
        message: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    Cancelled,
}

#[derive(Debug)]
enum GeminiFinalState {
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
struct GeminiAttemptState {
    final_text: String,
    raw_stdout: String,
    last_resume_selector: Option<String>,
    init_model: Option<String>,
    last_error_message: Option<String>,
    terminal_result_seen: bool,
    terminal_result_text: Option<String>,
    buffered_messages: Vec<StreamMessage>,
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
    if let Some(path) = crate::services::platform::resolve_binary_with_login_shell("gemini") {
        return Some(path);
    }

    let home = dirs::home_dir().unwrap_or_default();
    let mut known_paths = vec![home.join(".local/bin/gemini"), home.join("bin/gemini")];
    #[cfg(unix)]
    {
        known_paths.push(PathBuf::from("/usr/local/bin/gemini"));
        known_paths.push(PathBuf::from("/opt/homebrew/bin/gemini"));
    }
    #[cfg(windows)]
    {
        known_paths.push(home.join("AppData/Local/Programs/gemini/gemini.exe"));
        known_paths.push(PathBuf::from("C:/Program Files/gemini/gemini.exe"));
    }

    for path in &known_paths {
        if path.is_file() {
            return Some(path.display().to_string());
        }
    }

    None
}

fn get_gemini_path() -> Option<&'static str> {
    GEMINI_PATH.get_or_init(resolve_gemini_path).as_deref()
}

pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    let gemini_bin = get_gemini_path().ok_or_else(|| "Gemini CLI not found".to_string())?;
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut command = Command::new(gemini_bin);
    crate::services::platform::apply_runtime_path(&mut command);
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
) -> Result<(), String> {
    if remote_profile.is_some() {
        return Err(remote_profile_not_supported_message());
    }

    let gemini_bin = get_gemini_path().ok_or_else(|| "Gemini CLI not found".to_string())?;
    let prompt = compose_gemini_prompt(prompt, system_prompt, allowed_tools);
    let mut resume_selector = normalize_resume_selector(session_id)?;

    for attempt in 0..=GEMINI_MAX_SESSION_RETRIES {
        match execute_gemini_streaming_attempt(
            gemini_bin,
            &prompt,
            model,
            resume_selector.clone(),
            working_dir,
            sender.clone(),
            cancel_token.clone(),
        )? {
            GeminiAttemptResult::Completed | GeminiAttemptResult::Cancelled => return Ok(()),
            GeminiAttemptResult::RetrySession {
                message,
                stdout,
                stderr,
                exit_code,
            } => {
                if attempt < GEMINI_MAX_SESSION_RETRIES {
                    resume_selector = None;
                    continue;
                }
                let _ = sender.send(StreamMessage::Error {
                    message: format!(
                        "Gemini session could not be recovered after retry: {}",
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

fn execute_gemini_streaming_attempt(
    gemini_bin: &str,
    prompt: &str,
    model: Option<&str>,
    resume_selector: Option<String>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
) -> Result<GeminiAttemptResult, String> {
    let mut command = Command::new(gemini_bin);
    crate::services::platform::apply_runtime_path(&mut command);
    let mut child = command
        .args(build_exec_args(prompt, model, resume_selector.as_deref()))
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Gemini: {}", e))?;

    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(child.id());
    }

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
        claude::kill_child_tree(&mut child);
        let stderr = stderr_handle.join().unwrap_or_default();
        emit_cancellation_error(&sender, String::new(), stderr, None);
        return Ok(GeminiAttemptResult::Cancelled);
    }

    let mut state = GeminiAttemptState::new(resume_selector);
    let mut idle_ticks = 0;

    loop {
        if is_cancelled(cancel_token.as_deref()) {
            claude::kill_child_tree(&mut child);
            let stderr = stderr_handle.join().unwrap_or_default();
            emit_cancellation_error(&sender, state.raw_stdout, stderr, None);
            return Ok(GeminiAttemptResult::Cancelled);
        }

        match stdout_events.recv_timeout(GEMINI_STREAM_IDLE_TIMEOUT) {
            Ok(GeminiStreamEvent::Line(line)) => {
                idle_ticks = 0;
                process_gemini_stream_line(&line, &mut state);
            }
            Ok(GeminiStreamEvent::ReadError(message)) => {
                claude::kill_child_tree(&mut child);
                let stderr = stderr_handle.join().unwrap_or_default();
                return Ok(GeminiAttemptResult::RetrySession {
                    message,
                    stdout: state.raw_stdout,
                    stderr,
                    exit_code: None,
                });
            }
            Ok(GeminiStreamEvent::Eof) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if state.terminal_result_seen {
                    break;
                }
                idle_ticks += 1;
                if idle_ticks >= GEMINI_STREAM_IDLE_TICKS_BEFORE_RETRY {
                    claude::kill_child_tree(&mut child);
                    let stderr = stderr_handle.join().unwrap_or_default();
                    return Ok(GeminiAttemptResult::RetrySession {
                        message: format!(
                            "Gemini stream produced no output for {} seconds",
                            GEMINI_STREAM_IDLE_TIMEOUT.as_secs()
                                * GEMINI_STREAM_IDLE_TICKS_BEFORE_RETRY as u64
                        ),
                        stdout: state.raw_stdout,
                        stderr,
                        exit_code: None,
                    });
                }
            }
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("Failed waiting for Gemini: {}", e))?;
    let stderr = stderr_handle.join().unwrap_or_default();

    if is_cancelled(cancel_token.as_deref()) {
        emit_cancellation_error(&sender, state.raw_stdout, stderr, status.code());
        return Ok(GeminiAttemptResult::Cancelled);
    }

    match finalize_gemini_attempt(&mut state, stderr, status.code()) {
        GeminiFinalState::Done { result, session_id } => {
            flush_buffered_stream_messages(&sender, &mut state);
            let _ = sender.send(StreamMessage::Done { result, session_id });
            Ok(GeminiAttemptResult::Completed)
        }
        GeminiFinalState::Error {
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
            Ok(GeminiAttemptResult::Completed)
        }
        GeminiFinalState::RetrySession {
            message,
            stdout,
            stderr,
            exit_code,
        } => Ok(GeminiAttemptResult::RetrySession {
            message,
            stdout,
            stderr,
            exit_code,
        }),
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

fn process_gemini_stream_line(line: &str, state: &mut GeminiAttemptState) {
    if line.trim().is_empty() {
        return;
    }
    state.raw_stdout.push_str(line);
    state.raw_stdout.push('\n');

    let Ok(json) = serde_json::from_str::<Value>(line.trim()) else {
        return;
    };

    process_gemini_json_event(&json, state);
}

fn process_gemini_json_event(json: &Value, state: &mut GeminiAttemptState) {
    match json.get("type").and_then(|v| v.as_str()) {
        Some("init") => {
            if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
                state.last_resume_selector = observed_session_to_resume_selector(session_id);
                state.buffered_messages.push(StreamMessage::Init {
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
                state.final_text.push_str(content);
                state.buffered_messages.push(StreamMessage::Text {
                    content: content.to_string(),
                });
            }
        }
        Some("tool_use") => {
            if let Some(tool_use) = build_gemini_tool_use_message(json) {
                state.buffered_messages.push(tool_use);
            }
        }
        Some("tool_result") => {
            if let Some(tool_result) = build_gemini_tool_result_message(json) {
                state.buffered_messages.push(tool_result);
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
            state.buffered_messages.push(StreamMessage::StatusUpdate {
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
) -> GeminiFinalState {
    let final_text = std::mem::take(&mut state.final_text);
    let raw_stdout = std::mem::take(&mut state.raw_stdout);
    let last_resume_selector = state.last_resume_selector.take();
    let last_error_message = state.last_error_message.take();
    let terminal_result_seen = state.terminal_result_seen;
    let terminal_result_text = state.terminal_result_text.take();

    if terminal_result_seen {
        let result = final_text.trim().to_string();
        let result = if result.is_empty() {
            terminal_result_text.unwrap_or_default()
        } else {
            result
        };
        if result.is_empty() {
            return GeminiFinalState::Error {
                message: "Gemini emitted a terminal result without any response text".to_string(),
                stdout: raw_stdout,
                stderr,
                exit_code,
            };
        }
        return GeminiFinalState::Done {
            result,
            session_id: last_resume_selector,
        };
    }

    if let Some(message) = last_error_message {
        return GeminiFinalState::Error {
            message,
            stdout: raw_stdout,
            stderr,
            exit_code,
        };
    }

    if exit_code.unwrap_or(0) != 0 {
        return GeminiFinalState::RetrySession {
            message: derive_error_message(&raw_stdout, &stderr, exit_code, "Gemini"),
            stdout: raw_stdout,
            stderr,
            exit_code,
        };
    }

    if !stderr.trim().is_empty() {
        return GeminiFinalState::Error {
            message: derive_error_message(&raw_stdout, &stderr, exit_code, "Gemini"),
            stdout: raw_stdout,
            stderr,
            exit_code,
        };
    }

    GeminiFinalState::RetrySession {
        message: GEMINI_SESSION_DEAD_MESSAGE.to_string(),
        stdout: raw_stdout,
        stderr,
        exit_code,
    }
}

fn flush_buffered_stream_messages(sender: &Sender<StreamMessage>, state: &mut GeminiAttemptState) {
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
        message: GEMINI_CANCELLED_MESSAGE.to_string(),
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
    "NotSupported: Gemini provider does not support remote execution yet. Remove `remote_profile` or use a provider with remote support.".to_string()
}

fn compose_gemini_prompt(
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

    Err(
        "InvalidArgument: Gemini resume selector must be `latest` or a numeric session index"
            .to_string(),
    )
}

fn observed_session_to_resume_selector(_session_id: &str) -> Option<String> {
    Some(GEMINI_RESUME_LATEST.to_string())
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
        GEMINI_CANCELLED_MESSAGE, GEMINI_SESSION_DEAD_MESSAGE, GeminiAttemptState,
        GeminiFinalState, build_exec_args, build_gemini_tool_result_message,
        build_gemini_tool_use_message, emit_cancellation_error, execute_command_streaming,
        extract_gemini_error_message, extract_text_from_stream_output, finalize_gemini_attempt,
        flush_buffered_stream_messages, looks_like_uuid, normalize_resume_selector,
        observed_session_to_resume_selector, process_gemini_stream_line,
        remote_profile_not_supported_message,
    };
    use crate::services::claude::StreamMessage;
    use crate::services::remote::{RemoteAuth, RemoteProfile};
    use serde_json::json;
    use std::sync::mpsc;

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
    fn normalize_resume_selector_rejects_arbitrary_strings() {
        let error = normalize_resume_selector(Some("session-alpha")).unwrap_err();
        assert!(error.contains("InvalidArgument"));
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
        );

        assert!(state.final_text.is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn parser_empty_payload_is_ignored() {
        let (_tx, rx): (mpsc::Sender<StreamMessage>, mpsc::Receiver<StreamMessage>) =
            mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        process_gemini_stream_line("{}", &mut state);

        assert!(!state.terminal_result_seen);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn attempt_messages_are_buffered_until_flush() {
        let (tx, rx) = mpsc::channel();
        let mut state = GeminiAttemptState::new(None);
        process_gemini_stream_line(
            r#"{"type":"init","session_id":"aa678e6b-c6d3-4dd2-9197-58580c00cc6c","model":"gemini-2.5-flash"}"#,
            &mut state,
        );
        process_gemini_stream_line(
            r#"{"type":"message","role":"assistant","content":"hello"}"#,
            &mut state,
        );

        assert!(rx.try_recv().is_err());
        flush_buffered_stream_messages(&tx, &mut state);

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
    fn terminal_result_is_authoritative_even_if_error_seen() {
        let mut state = GeminiAttemptState::new(Some("latest".to_string()));
        state.last_error_message = Some("quota exceeded".to_string());
        state.final_text = "done".to_string();
        state.terminal_result_seen = true;

        match finalize_gemini_attempt(&mut state, String::new(), Some(0)) {
            GeminiFinalState::Done { result, session_id } => {
                assert_eq!(result, "done");
                assert_eq!(session_id.as_deref(), Some("latest"));
            }
            other => panic!("expected Done, got {:?}", other),
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
            GeminiFinalState::RetrySession { message, .. } => {
                assert_eq!(message, GEMINI_SESSION_DEAD_MESSAGE);
            }
            other => panic!("expected RetrySession, got {:?}", other),
        }
    }

    #[test]
    fn cancellation_emits_single_error_message() {
        let (tx, rx) = mpsc::channel();
        emit_cancellation_error(&tx, "out".to_string(), "err".to_string(), Some(130));

        match rx.recv().unwrap() {
            StreamMessage::Error {
                message,
                stdout,
                stderr,
                exit_code,
            } => {
                assert_eq!(message, GEMINI_CANCELLED_MESSAGE);
                assert_eq!(stdout, "out");
                assert_eq!(stderr, "err");
                assert_eq!(exit_code, Some(130));
            }
            other => panic!("expected Error, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn remote_profile_not_supported_message_has_guidance() {
        let message = remote_profile_not_supported_message();
        assert!(message.contains("NotSupported"));
        assert!(message.contains("remote_profile"));
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
        )
        .unwrap_err();

        assert!(error.contains("NotSupported"));
    }
}
