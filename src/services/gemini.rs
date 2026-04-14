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
};
use crate::services::provider_runtime::{LineStreamEvent, spawn_line_stream_reader};
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
const GEMINI_TRUSTED_FOLDERS_PATH: &str = ".gemini/trustedFolders.json";
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

pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    execute_command_simple_cancellable(prompt, None)
}

pub fn execute_command_simple_with_timeout(
    prompt: &str,
    timeout: Duration,
    label: &str,
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
    let output = command
        .args(&exec_args.args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Gemini: {}", e))
        .and_then(|child| wait_with_output_timeout(child, timeout, label))?;

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
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(stripped) = raw.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(stripped))
            .unwrap_or_else(|| PathBuf::from(raw));
    }
    PathBuf::from(raw)
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
        GeminiExecArgs, GeminiStreamEvent, GeminiStreamLoopResult, build_exec_args,
        build_gemini_tool_result_message, build_gemini_tool_use_message,
        collect_gemini_stream_events, execute_command_streaming, extract_gemini_error_message,
        extract_text_from_stream_output, finalize_gemini_attempt, gemini_trust_rule_for_entry,
        gemini_trust_status_allows_root, looks_like_uuid, normalize_resume_selector,
        observed_session_to_resume_selector, process_gemini_stream_line,
        remote_profile_not_supported_message, run_gemini_streaming_attempts,
        select_gemini_working_dir,
    };
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::provider::{
        CancelToken, StreamAttemptFailure, StreamAttemptResult, StreamFinalState,
    };
    use crate::services::remote::{RemoteAuth, RemoteProfile};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;

    fn admin_policy_path(args: &GeminiExecArgs) -> String {
        args.args
            .windows(2)
            .find(|pair| pair[0] == "--admin-policy")
            .map(|pair| pair[1].clone())
            .expect("readonly Gemini must pass an admin policy")
    }

    #[test]
    fn build_exec_args_includes_resume_when_session_present() {
        let args = build_exec_args("hello", Some("gemini-2.5-flash"), Some("latest"), false)
            .expect("args");
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["--resume", "latest"])
        );
        assert!(args.args.windows(2).any(|pair| pair == ["-p", "hello"]));
        assert!(
            !args
                .args
                .windows(2)
                .any(|pair| pair == ["-m", "gemini-2.5-flash"])
        );
    }

    #[test]
    fn build_exec_args_includes_model_for_fresh_session() {
        let args = build_exec_args("hello", Some("gemini-2.5-flash"), None, false).expect("args");
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["-m", "gemini-2.5-flash"])
        );
        assert!(!args.args.iter().any(|arg| arg == "--resume"));
    }

    #[test]
    fn build_exec_args_uses_default_mode_with_admin_policy_for_readonly_sessions() {
        let args = build_exec_args("hello", Some("gemini-2.5-flash"), None, true).expect("args");
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["--sandbox", "true"])
        );
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["--approval-mode", "default"])
        );
        let policy_path = admin_policy_path(&args);
        let policy = std::fs::read_to_string(policy_path).expect("policy file should exist");
        assert!(policy.contains("read_many_files"));
        assert!(policy.contains("toolName = \"*\""));
        assert!(!policy.contains("modes ="));
        assert!(!args.args.iter().any(|arg| arg == "-y"));
    }

    #[test]
    fn build_exec_args_uses_unique_admin_policy_path_per_readonly_request() {
        let first = build_exec_args("hello", Some("gemini-2.5-flash"), None, true).expect("args");
        let second =
            build_exec_args("hello again", Some("gemini-2.5-flash"), None, true).expect("args");

        let first_path = admin_policy_path(&first);
        let second_path = admin_policy_path(&second);
        assert_ne!(first_path, second_path);
        assert!(std::path::Path::new(&first_path).exists());
        assert!(std::path::Path::new(&second_path).exists());
    }

    #[test]
    fn build_exec_args_uses_approval_mode_yolo_without_legacy_y_flag() {
        let args = build_exec_args("hello", Some("gemini-2.5-flash"), None, false).expect("args");
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["--approval-mode", "yolo"])
        );
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["--sandbox", "false"])
        );
        assert!(!args.args.iter().any(|arg| arg == "-y" || arg == "--yolo"));
    }

    #[test]
    fn select_gemini_working_dir_keeps_trusted_descendant() {
        let trusted = vec![super::GeminiTrustRule::allow(PathBuf::from(
            "/Users/kunkun",
        ))];
        let resolved = select_gemini_working_dir(
            PathBuf::from("/Users/kunkun/kunkunGames/agentdesk"),
            Some(PathBuf::from("/Users/kunkun")),
            &trusted,
        )
        .unwrap();
        assert_eq!(
            resolved,
            PathBuf::from("/Users/kunkun/kunkunGames/agentdesk")
        );
    }

    #[test]
    fn select_gemini_working_dir_falls_back_from_root_to_trusted_home() {
        let trusted = vec![super::GeminiTrustRule::allow(PathBuf::from(
            "/Users/kunkun",
        ))];
        let resolved = select_gemini_working_dir(
            PathBuf::from("/"),
            Some(PathBuf::from("/Users/kunkun")),
            &trusted,
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/Users/kunkun"));
    }

    #[cfg(windows)]
    #[test]
    fn select_gemini_working_dir_falls_back_from_drive_root_to_trusted_home() {
        let trusted = vec![super::GeminiTrustRule::allow(PathBuf::from(
            r"C:\Users\kunkun",
        ))];
        let resolved = select_gemini_working_dir(
            PathBuf::from(r"C:\"),
            Some(PathBuf::from(r"C:\Users\kunkun")),
            &trusted,
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from(r"C:\Users\kunkun"));
    }

    #[test]
    fn select_gemini_working_dir_rejects_untrusted_directory() {
        let trusted = vec![super::GeminiTrustRule::allow(PathBuf::from(
            "/Users/kunkun",
        ))];
        let error = select_gemini_working_dir(
            PathBuf::from("/private/tmp/example"),
            Some(PathBuf::from("/Users/kunkun")),
            &trusted,
        )
        .unwrap_err();
        assert!(error.contains("/private/tmp/example"));
        assert!(error.contains("not trusted"));
    }

    #[test]
    fn gemini_trust_status_accepts_folder_and_parent_entries() {
        assert!(gemini_trust_status_allows_root(&json!("TRUST_FOLDER")));
        assert!(gemini_trust_status_allows_root(&json!("TRUST_PARENT")));
        assert!(!gemini_trust_status_allows_root(&json!("UNTRUSTED")));
    }

    #[test]
    fn gemini_trust_rule_maps_supported_statuses() {
        assert_eq!(
            gemini_trust_rule_for_entry("/private/tmp/worktree-a", &json!("TRUST_PARENT")).unwrap(),
            super::GeminiTrustRule::allow(PathBuf::from("/private/tmp"))
        );
        assert_eq!(
            gemini_trust_rule_for_entry("/private/tmp/worktree-a", &json!("TRUST_FOLDER")).unwrap(),
            super::GeminiTrustRule::allow(PathBuf::from("/private/tmp/worktree-a"))
        );
        assert_eq!(
            gemini_trust_rule_for_entry("/private/tmp/worktree-b", &json!("DO_NOT_TRUST")).unwrap(),
            super::GeminiTrustRule::deny(PathBuf::from("/private/tmp/worktree-b"))
        );
    }

    #[test]
    fn select_gemini_working_dir_accepts_trust_parent_sibling() {
        let trusted = vec![
            gemini_trust_rule_for_entry("/private/tmp/worktree-a", &json!("TRUST_PARENT")).unwrap(),
        ];
        let resolved = select_gemini_working_dir(
            PathBuf::from("/private/tmp/worktree-b/project"),
            Some(PathBuf::from("/Users/kunkun")),
            &trusted,
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/private/tmp/worktree-b/project"));
    }

    #[test]
    fn select_gemini_working_dir_rejects_denied_child_under_trusted_parent() {
        let trusted = vec![
            gemini_trust_rule_for_entry("/private/tmp/worktree-a", &json!("TRUST_PARENT")).unwrap(),
            gemini_trust_rule_for_entry("/private/tmp/worktree-b", &json!("DO_NOT_TRUST")).unwrap(),
        ];
        let error = select_gemini_working_dir(
            PathBuf::from("/private/tmp/worktree-b/project"),
            Some(PathBuf::from("/Users/kunkun")),
            &trusted,
        )
        .unwrap_err();
        assert!(error.contains("/private/tmp/worktree-b/project"));
        assert!(error.contains("not trusted"));
    }

    #[cfg(unix)]
    #[test]
    fn select_gemini_working_dir_keeps_non_utf8_trusted_path() {
        use std::os::unix::ffi::OsStringExt;

        let trusted_path = PathBuf::from(std::ffi::OsString::from_vec(
            b"/private/tmp/gemini-\xff".to_vec(),
        ));
        let trusted = vec![super::GeminiTrustRule::allow(trusted_path.clone())];
        let resolved = select_gemini_working_dir(
            trusted_path.clone(),
            Some(PathBuf::from("/Users/kunkun")),
            &trusted,
        )
        .unwrap();
        assert_eq!(resolved, trusted_path);
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
