use serde_json::Value;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::services::agent_protocol::{StreamMessage, is_valid_session_id};
use crate::services::process::{
    configure_child_process_group, kill_child_tree, wait_with_output_timeout,
};
use crate::services::provider::{
    CancelToken, ProviderKind, StreamAttemptFailure, StreamAttemptResult, StreamFinalState,
    cancel_requested, is_readonly_tool_policy, register_child_pid, run_retrying_stream_attempts,
    spawn_cancel_watchdog,
};
use crate::services::provider_runtime::{LineStreamEvent, spawn_line_stream_reader};
use crate::services::remote::RemoteProfile;

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";
const GEMINI_RESUME_LATEST: &str = "latest";
const GEMINI_SESSION_DEAD_MESSAGE: &str = "Gemini stream ended without a terminal result";
const GEMINI_INVALID_RESUME_SELECTOR_MESSAGE: &str = "InvalidArgument: Gemini resume selector must be `latest`, a numeric session index, or a UUID-like Gemini session reference";
const GEMINI_NO_PREVIOUS_SESSIONS_MESSAGE: &str = "No previous sessions found for this project.";
const GEMINI_NO_SESSIONS_FOUND_MESSAGE: &str = "No sessions found for this project.";
const GEMINI_DELETE_CURRENT_SESSION_MESSAGE: &str = "Cannot delete the current active session.";
const GEMINI_STREAM_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const GEMINI_STREAM_IDLE_WATCHDOG: Duration = Duration::from_secs(120);
const GEMINI_STREAM_STARTUP_WATCHDOG: Duration = Duration::from_secs(60);
const GEMINI_MAX_SESSION_RETRIES: usize = 1;
const GEMINI_TRUSTED_FOLDERS_PATH: &str = ".gemini/trustedFolders.json";
const GEMINI_MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(30);
const GEMINI_MEETING_READONLY_POLICY: &str = r#"
[[rule]]
toolName = [
  "glob",
  "grep",
  "grep_search",
  "list_directory",
  "read_file",
  "read_many_files"
]
decision = "allow"
priority = 950

[[rule]]
toolName = "*"
decision = "deny"
priority = 900
denyMessage = "AgentDesk meeting_readonly mode allows only filesystem read/search tools."
"#;

type GeminiStreamEvent = LineStreamEvent;

static GEMINI_MEETING_READONLY_POLICY_COUNTER: AtomicU64 = AtomicU64::new(0);

struct GeminiExecArgs {
    args: Vec<String>,
    _readonly_policy_path: Option<PathBuf>,
}

impl Drop for GeminiExecArgs {
    fn drop(&mut self) {
        if let Some(path) = self._readonly_policy_path.as_ref() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GeminiTrustRuleKind {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GeminiTrustRule {
    root: PathBuf,
    kind: GeminiTrustRuleKind,
}

impl GeminiTrustRule {
    fn allow(root: PathBuf) -> Self {
        Self {
            root,
            kind: GeminiTrustRuleKind::Allow,
        }
    }

    fn deny(root: PathBuf) -> Self {
        Self {
            root,
            kind: GeminiTrustRuleKind::Deny,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum GeminiStreamLoopResult {
    Eof,
    RetrySession { message: String },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiResumeSelectorSource {
    Fresh,
    Latest,
    Index,
    UuidCoerced,
    Rejected,
}

impl GeminiResumeSelectorSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Latest => "latest",
            Self::Index => "index",
            Self::UuidCoerced => "uuid-coerced",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Default)]
struct GeminiAttemptState {
    final_text: String,
    raw_stdout: String,
    last_resume_selector: Option<String>,
    last_raw_session_id: Option<String>,
    init_model: Option<String>,
    last_error_message: Option<String>,
    terminal_result_seen: bool,
    terminal_result_text: Option<String>,
    meaningful_progress_seen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GeminiProjectSession {
    pub index: usize,
    pub title: String,
    pub relative_time: String,
    pub is_current_session: bool,
    pub session_id: String,
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

pub fn execute_command_simple_cancellable(
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<String, String> {
    let resolution = resolve_gemini_binary();
    let gemini_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Gemini CLI not found".to_string())?;
    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let working_dir = resolve_gemini_requested_dir(current_dir)?;
    let exec_args = build_exec_args(prompt, None, None, false)?;
    let mut command = Command::new(&gemini_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    configure_child_process_group(&mut command);
    let mut child = command
        .args(&exec_args.args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Gemini: {}", e))?;

    register_child_pid(cancel_token, child.id());
    if is_cancelled(cancel_token) {
        kill_child_tree(&mut child);
        return Err("Gemini request cancelled".to_string());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to read Gemini output: {}", e))?;

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

pub(crate) fn list_project_sessions(
    working_dir: &str,
) -> Result<Vec<GeminiProjectSession>, String> {
    let resolved_working_dir = resolve_gemini_working_dir(working_dir)?;
    let (stdout, stderr, exit_code) = run_gemini_management_command(
        &resolved_working_dir,
        &["--list-sessions".to_string()],
        "Gemini --list-sessions",
    )?;

    match parse_gemini_session_list_output(&stdout) {
        Ok(sessions) => {
            if !stderr.trim().is_empty() {
                tracing::warn!(
                    provider = "gemini",
                    stderr = stderr.trim(),
                    "Gemini --list-sessions emitted stderr while returning parseable output"
                );
            }
            Ok(sessions)
        }
        Err(parse_error) => {
            if !stderr.trim().is_empty() {
                Err(derive_error_message(
                    &stdout,
                    &stderr,
                    exit_code,
                    "Gemini --list-sessions",
                ))
            } else {
                Err(parse_error)
            }
        }
    }
}

pub(crate) fn delete_project_session(
    working_dir: &str,
    session_identifier: &str,
) -> Result<String, String> {
    let resolved_working_dir = resolve_gemini_working_dir(working_dir)?;
    let sessions = list_project_sessions(working_dir)?;
    let normalized_identifier = normalize_gemini_delete_identifier(session_identifier, &sessions)?;
    let (stdout, stderr, exit_code) = run_gemini_management_command(
        &resolved_working_dir,
        &[
            "--delete-session".to_string(),
            normalized_identifier.clone(),
        ],
        "Gemini --delete-session",
    )?;

    let trimmed_stdout = stdout.trim();
    if trimmed_stdout.starts_with("Deleted session ") {
        if !stderr.trim().is_empty() {
            tracing::warn!(
                provider = "gemini",
                stderr = stderr.trim(),
                "Gemini --delete-session emitted stderr alongside a success message"
            );
        }
        return Ok(trimmed_stdout.to_string());
    }

    if !stderr.trim().is_empty() {
        return Err(derive_error_message(
            &stdout,
            &stderr,
            exit_code,
            "Gemini --delete-session",
        ));
    }

    Err(format!(
        "Gemini --delete-session returned an unrecognized response for selector `{normalized_identifier}`"
    ))
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

    let (resume_selector, selector_source) = normalize_resume_selector_with_source(session_id)
        .map_err(|error| {
            log_gemini_selector_resolution(
                "resume-input",
                GeminiResumeSelectorSource::Rejected,
                session_id,
                None,
            );
            error
        })?;
    log_gemini_selector_resolution(
        "resume-input",
        selector_source,
        session_id,
        resume_selector.as_deref(),
    );
    if is_cancelled(cancel_token.as_deref()) {
        return Ok(());
    }

    let resolution = resolve_gemini_binary();
    let gemini_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Gemini CLI not found".to_string())?;
    let resolved_working_dir = resolve_gemini_working_dir(working_dir)?;
    let prompt = compose_gemini_prompt(prompt, system_prompt, allowed_tools);
    let readonly_mode = is_readonly_tool_policy(allowed_tools);
    run_gemini_streaming_attempts(&sender, resume_selector, |resume_selector| {
        execute_gemini_streaming_attempt(
            &gemini_bin,
            &resolution,
            &prompt,
            model,
            resume_selector,
            &resolved_working_dir,
            sender.clone(),
            cancel_token.clone(),
            readonly_mode,
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

#[allow(clippy::too_many_arguments)]
fn execute_gemini_streaming_attempt(
    gemini_bin: &str,
    resolution: &crate::services::platform::BinaryResolution,
    prompt: &str,
    model: Option<&str>,
    resume_selector: Option<String>,
    working_dir: &Path,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    readonly_mode: bool,
) -> Result<StreamAttemptResult, String> {
    let mut command = Command::new(gemini_bin);
    crate::services::platform::apply_binary_resolution(&mut command, resolution);
    configure_child_process_group(&mut command);
    let exec_args = build_exec_args(prompt, model, resume_selector.as_deref(), readonly_mode)?;
    let mut child = command
        .args(&exec_args.args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Gemini: {}", e))?;

    register_child_pid(cancel_token.as_deref(), child.id());
    let _cancel_watchdog = spawn_cancel_watchdog(cancel_token.clone(), "gemini-direct-stream");
    if cancel_requested(cancel_token.as_deref()) {
        kill_child_tree(&mut child);
        return Ok(StreamAttemptResult::Cancelled);
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Gemini stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture Gemini stderr".to_string())?;
    let stdout_events = spawn_line_stream_reader(stdout, "Gemini");
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

#[allow(clippy::too_many_arguments)]
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
                state.last_raw_session_id = Some(session_id.to_string());
                // Preserve existing numeric/latest resume selector unless Gemini emits a
                // stronger verified selector. UUID-like values are retained as raw metadata
                // but coerced to a safe executable selector because Gemini 0.38.0 silently
                // starts a new session when `--resume <missing-uuid>` is accepted.
                let observed = observed_session_to_resume_selector_with_source(session_id);
                let existing_is_resumable =
                    state.last_resume_selector.as_ref().map_or(false, |s| {
                        s == GEMINI_RESUME_LATEST || s.chars().all(|c| c.is_ascii_digit())
                    });
                if !existing_is_resumable
                    || observed
                        .as_ref()
                        .map_or(false, |(selector, _)| selector != GEMINI_RESUME_LATEST)
                {
                    state.last_resume_selector =
                        observed.as_ref().map(|(selector, _)| selector.clone());
                }
                if let Some((selector, selector_source)) = observed.as_ref() {
                    log_gemini_selector_resolution(
                        "init-observed",
                        *selector_source,
                        Some(session_id),
                        Some(selector.as_str()),
                    );
                }
                let _ = sender.send(StreamMessage::Init {
                    session_id: state
                        .last_resume_selector
                        .clone()
                        .unwrap_or_else(|| GEMINI_RESUME_LATEST.to_string()),
                    raw_session_id: state.last_raw_session_id.clone(),
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
                cache_create_tokens: None,
                cache_read_tokens: None,
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

fn run_gemini_management_command(
    working_dir: &Path,
    args: &[String],
    label: &str,
) -> Result<(String, String, Option<i32>), String> {
    let resolution = resolve_gemini_binary();
    let gemini_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Gemini CLI not found".to_string())?;

    let mut command = Command::new(&gemini_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    configure_child_process_group(&mut command);
    let output = command
        .args(args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Gemini: {e}"))
        .and_then(|child| wait_with_output_timeout(child, GEMINI_MANAGEMENT_TIMEOUT, label))?;

    Ok((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
        output.status.code(),
    ))
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

fn resolve_gemini_requested_dir(requested_dir: PathBuf) -> Result<PathBuf, String> {
    let trust_rules = gemini_trust_rules();
    select_gemini_working_dir(requested_dir, dirs::home_dir(), &trust_rules)
}

fn resolve_gemini_working_dir(working_dir: &str) -> Result<PathBuf, String> {
    resolve_gemini_requested_dir(expand_gemini_working_dir(working_dir))
}

fn expand_gemini_working_dir(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw.is_empty() {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    }
    crate::runtime_layout::expand_user_path(raw).unwrap_or_else(|| PathBuf::from(raw))
}

fn gemini_trust_rules() -> Vec<GeminiTrustRule> {
    let Some(path) = dirs::home_dir().map(|home| home.join(GEMINI_TRUSTED_FOLDERS_PATH)) else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<Value>(&contents) else {
        return Vec::new();
    };
    let Some(entries) = json.as_object() else {
        return Vec::new();
    };

    entries
        .iter()
        .filter_map(|(path, status)| gemini_trust_rule_for_entry(path, status))
        .collect()
}

fn gemini_trust_status_allows_root(status: &Value) -> bool {
    matches!(status.as_str(), Some("TRUST_FOLDER") | Some("TRUST_PARENT"))
}

fn gemini_trust_rule_for_entry(path: &str, status: &Value) -> Option<GeminiTrustRule> {
    if matches!(status.as_str(), Some("TRUST_PARENT")) {
        let rule_path = Path::new(path);
        let parent = rule_path.parent().unwrap_or(rule_path);
        return Some(GeminiTrustRule::allow(normalize_gemini_path(parent)));
    }
    if gemini_trust_status_allows_root(status) {
        return Some(GeminiTrustRule::allow(normalize_gemini_path(path)));
    }
    if matches!(status.as_str(), Some("DO_NOT_TRUST")) {
        return Some(GeminiTrustRule::deny(normalize_gemini_path(path)));
    }
    None
}

fn select_gemini_working_dir(
    requested_dir: PathBuf,
    home_dir: Option<PathBuf>,
    trust_rules: &[GeminiTrustRule],
) -> Result<PathBuf, String> {
    if path_is_trusted_by_gemini(&requested_dir, trust_rules) {
        return Ok(requested_dir);
    }

    if requested_dir.has_root() && requested_dir.parent().is_none() {
        if let Some(home_dir) = home_dir {
            let normalized_home = normalize_gemini_path(home_dir);
            if path_is_trusted_by_gemini(&normalized_home, trust_rules) {
                return Ok(normalized_home);
            }
        }
        return Err(format!(
            "Gemini cannot use `{}` as the working directory in headless mode because it is not trusted in `~/{}`. Trust that filesystem root or switch the session to a trusted folder.",
            requested_dir.display(),
            GEMINI_TRUSTED_FOLDERS_PATH
        ));
    }

    Err(format!(
        "Gemini working directory `{}` is not trusted in `~/{}`. Switch to a trusted folder or add it to Gemini trusted folders.",
        requested_dir.display(),
        GEMINI_TRUSTED_FOLDERS_PATH
    ))
}

fn path_is_trusted_by_gemini(path: &Path, trust_rules: &[GeminiTrustRule]) -> bool {
    let normalized = normalize_gemini_path(path);
    let most_specific_rule = trust_rules
        .iter()
        .filter(|rule| normalized.starts_with(&rule.root))
        .max_by_key(|rule| rule.root.components().count());
    matches!(
        most_specific_rule.map(|rule| &rule.kind),
        Some(GeminiTrustRuleKind::Allow)
    )
}

fn normalize_gemini_path(path: impl AsRef<Path>) -> PathBuf {
    std::fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf())
}

fn build_exec_args(
    prompt: &str,
    model: Option<&str>,
    session_id: Option<&str>,
    readonly_mode: bool,
) -> Result<GeminiExecArgs, String> {
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
    let mut readonly_policy_path = None;
    if readonly_mode {
        args.push("--sandbox".to_string());
        args.push("true".to_string());
        args.push("--approval-mode".to_string());
        args.push("default".to_string());
        args.push("--admin-policy".to_string());
        let policy_path = ensure_gemini_meeting_readonly_policy_file()?;
        args.push(policy_path.to_string_lossy().to_string());
        readonly_policy_path = Some(policy_path);
    } else {
        args.push("--approval-mode".to_string());
        args.push("yolo".to_string());
        args.push("--sandbox".to_string());
        args.push("false".to_string());
    }
    Ok(GeminiExecArgs {
        args,
        _readonly_policy_path: readonly_policy_path,
    })
}

fn ensure_gemini_meeting_readonly_policy_file() -> Result<PathBuf, String> {
    let unique = GEMINI_MEETING_READONLY_POLICY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "agentdesk-gemini-meeting-readonly-{}-{}-{}.toml",
        std::process::id(),
        timestamp,
        unique
    ));
    std::fs::write(&path, GEMINI_MEETING_READONLY_POLICY).map_err(|error| {
        format!(
            "Failed to write Gemini meeting read-only admin policy at {}: {}",
            path.display(),
            error
        )
    })?;
    Ok(path)
}

fn log_gemini_selector_resolution(
    phase: &str,
    source: GeminiResumeSelectorSource,
    raw_input: Option<&str>,
    effective_selector: Option<&str>,
) {
    let raw_input = raw_input.unwrap_or("<none>");
    let effective_selector = effective_selector.unwrap_or("<fresh>");
    match source {
        GeminiResumeSelectorSource::UuidCoerced | GeminiResumeSelectorSource::Rejected => {
            tracing::warn!(
                provider = "gemini",
                selector_phase = phase,
                selector_source = source.as_str(),
                raw_input,
                effective_selector,
                "Gemini resume selector required coercion or rejection"
            );
        }
        _ => {
            tracing::debug!(
                provider = "gemini",
                selector_phase = phase,
                selector_source = source.as_str(),
                raw_input,
                effective_selector,
                "Gemini resume selector resolved"
            );
        }
    }
}

fn normalize_resume_selector_with_source(
    session_id: Option<&str>,
) -> Result<(Option<String>, GeminiResumeSelectorSource), String> {
    let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok((None, GeminiResumeSelectorSource::Fresh));
    };

    if session_id.eq_ignore_ascii_case(GEMINI_RESUME_LATEST) {
        return Ok((
            Some(GEMINI_RESUME_LATEST.to_string()),
            GeminiResumeSelectorSource::Latest,
        ));
    }

    if session_id.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok((
            Some(session_id.to_string()),
            GeminiResumeSelectorSource::Index,
        ));
    }

    if looks_like_uuid(session_id) {
        // Gemini 0.38.0 documents UUID-like references, but live runtime probes show
        // that `--resume <missing-uuid>` exits 0 and silently creates a fresh session.
        // Keep the raw UUID for telemetry/persistence while coercing execution to a
        // verified selector that will not silently fork history.
        return Ok((
            Some(GEMINI_RESUME_LATEST.to_string()),
            GeminiResumeSelectorSource::UuidCoerced,
        ));
    }

    if is_common_session_metadata(session_id) {
        return Err(GEMINI_INVALID_RESUME_SELECTOR_MESSAGE.to_string());
    }

    Err(GEMINI_INVALID_RESUME_SELECTOR_MESSAGE.to_string())
}

fn observed_session_to_resume_selector_with_source(
    session_id: &str,
) -> Option<(String, GeminiResumeSelectorSource)> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }

    if session_id.eq_ignore_ascii_case(GEMINI_RESUME_LATEST) {
        return Some((
            GEMINI_RESUME_LATEST.to_string(),
            GeminiResumeSelectorSource::Latest,
        ));
    }

    if session_id.chars().all(|ch| ch.is_ascii_digit()) {
        return Some((session_id.to_string(), GeminiResumeSelectorSource::Index));
    }

    if looks_like_uuid(session_id) || is_common_session_metadata(session_id) {
        return Some((
            GEMINI_RESUME_LATEST.to_string(),
            GeminiResumeSelectorSource::UuidCoerced,
        ));
    }

    None
}

fn is_common_session_metadata(session_id: &str) -> bool {
    let session_id = session_id.trim();
    !session_id.is_empty() && is_valid_session_id(session_id)
}

fn parse_gemini_session_list_output(output: &str) -> Result<Vec<GeminiProjectSession>, String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Err("Gemini session list output was empty".to_string());
    }
    if trimmed == GEMINI_NO_PREVIOUS_SESSIONS_MESSAGE {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Available sessions for this project") {
            continue;
        }
        sessions.push(parse_gemini_session_list_line(trimmed)?);
    }

    if sessions.is_empty() {
        return Err("Gemini session list output did not contain any parseable entries".to_string());
    }

    Ok(sessions)
}

fn parse_gemini_session_list_line(line: &str) -> Result<GeminiProjectSession, String> {
    let dot_index = line
        .find(". ")
        .ok_or_else(|| format!("Gemini session line is missing an index separator: {line}"))?;
    let index = line[..dot_index]
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("Gemini session line has an invalid index: {line}"))?;

    let remainder = line[dot_index + 2..].trim();
    let closing_bracket = remainder.rfind(']').ok_or_else(|| {
        format!("Gemini session line is missing a closing session id bracket: {line}")
    })?;
    let opening_bracket = remainder[..closing_bracket].rfind('[').ok_or_else(|| {
        format!("Gemini session line is missing an opening session id bracket: {line}")
    })?;
    let session_id = remainder[opening_bracket + 1..closing_bracket].trim();
    if session_id.is_empty() {
        return Err(format!(
            "Gemini session line has an empty session id: {line}"
        ));
    }

    let before_session_id = remainder[..opening_bracket].trim_end();
    let closing_paren = before_session_id.rfind(')').ok_or_else(|| {
        format!("Gemini session line is missing a closing metadata parenthesis: {line}")
    })?;
    let opening_paren = before_session_id[..closing_paren]
        .rfind('(')
        .ok_or_else(|| {
            format!("Gemini session line is missing an opening metadata parenthesis: {line}")
        })?;
    let title = before_session_id[..opening_paren].trim_end();
    if title.is_empty() {
        return Err(format!("Gemini session line has an empty title: {line}"));
    }
    let metadata = before_session_id[opening_paren + 1..closing_paren].trim();
    if metadata.is_empty() {
        return Err(format!("Gemini session line has empty metadata: {line}"));
    }

    let (relative_time, is_current_session) =
        if let Some(relative_time) = metadata.strip_suffix(", current") {
            (relative_time.trim().to_string(), true)
        } else {
            (metadata.to_string(), false)
        };

    Ok(GeminiProjectSession {
        index,
        title: title.to_string(),
        relative_time,
        is_current_session,
        session_id: session_id.to_string(),
    })
}

fn normalize_gemini_delete_identifier(
    session_identifier: &str,
    sessions: &[GeminiProjectSession],
) -> Result<String, String> {
    let session_identifier = session_identifier.trim();
    if session_identifier.is_empty() {
        return Err(
            "Invalid session identifier \"\". Use --list-sessions to see available sessions."
                .to_string(),
        );
    }
    if sessions.is_empty() {
        return Err(GEMINI_NO_SESSIONS_FOUND_MESSAGE.to_string());
    }

    if session_identifier.chars().all(|ch| ch.is_ascii_digit()) {
        let index = session_identifier.parse::<usize>().map_err(|_| {
            format!(
                "Invalid session identifier \"{session_identifier}\". Use --list-sessions to see available sessions."
            )
        })?;
        let Some(session) = sessions.iter().find(|session| session.index == index) else {
            return Err(format!(
                "Invalid session identifier \"{session_identifier}\". Use --list-sessions to see available sessions."
            ));
        };
        if session.is_current_session {
            return Err(GEMINI_DELETE_CURRENT_SESSION_MESSAGE.to_string());
        }
        return Ok(session_identifier.to_string());
    }

    let Some(session) = sessions
        .iter()
        .find(|session| session.session_id == session_identifier)
    else {
        return Err(format!(
            "Invalid session identifier \"{session_identifier}\". Use --list-sessions to see available sessions."
        ));
    };
    if session.is_current_session {
        return Err(GEMINI_DELETE_CURRENT_SESSION_MESSAGE.to_string());
    }

    Ok(session_identifier.to_string())
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
        tool_use_id: gemini_tool_call_id(json),
    })
}

/// Extracts a Gemini tool-call identifier so a `ToolResult` can be paired back
/// to its `ToolUse` instead of relying on FIFO ordering.
fn gemini_tool_call_id(json: &Value) -> Option<String> {
    ["call_id", "tool_call_id", "callId", "id"]
        .into_iter()
        .find_map(|key| json.get(key).and_then(|v| v.as_str()))
        .map(str::to_string)
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
        tool_use_id: gemini_tool_call_id(json),
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
mod path_expansion_tests {
    use super::expand_gemini_working_dir;
    use std::path::PathBuf;

    #[test]
    fn expand_gemini_working_dir_keeps_existing_trimmed_relative_behavior() {
        assert_eq!(
            expand_gemini_working_dir("  relative/project  "),
            PathBuf::from("relative/project")
        );
    }

    #[test]
    fn expand_gemini_working_dir_reuses_runtime_tilde_expansion() {
        let Some(home) = dirs::home_dir() else {
            return;
        };

        assert_eq!(
            expand_gemini_working_dir("~/agentdesk"),
            home.join("agentdesk")
        );

        #[cfg(windows)]
        assert_eq!(
            expand_gemini_working_dir(r"~\agentdesk"),
            home.join("agentdesk")
        );
    }
}
