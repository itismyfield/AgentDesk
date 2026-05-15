use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
#[cfg(unix)]
use std::sync::{Arc, LazyLock, Mutex};
#[cfg(unix)]
use std::time::Duration;

use crate::services::agent_protocol::{StreamMessage, is_valid_session_id};
use crate::services::discord::restart_report::{
    RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
};
use crate::services::process::{kill_child_tree, kill_pid_tree, shell_escape};
use crate::services::provider::{
    CancelToken, ProviderKind, ReadOutputResult, SessionProbe, cancel_requested,
    fold_read_output_result, register_child_pid,
};
use crate::services::provider_hosting::ProviderSessionDriver;
use crate::services::remote::RemoteProfile;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::services::session_backend::parse_stream_message;
use crate::services::session_backend::{
    StreamLineState, insert_process_session, observe_stream_context,
    parse_assistant_extra_tool_uses, parse_stream_message_with_state, process_session_is_alive,
    process_session_pid, process_session_probe, read_output_file_until_result,
    remove_process_session, send_process_session_input, terminate_process_handle,
};
#[cfg(unix)]
use crate::services::tmux_diagnostics::{
    record_tmux_exit_reason, should_recreate_session_after_followup_fifo_error,
    tmux_session_exists, tmux_session_has_live_pane,
};

#[cfg(unix)]
const CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS: usize = 2;
#[cfg(unix)]
const CLAUDE_TUI_FRESH_PROMPT_READY_BACKOFF_BASE: Duration = Duration::from_secs(5);

#[cfg(unix)]
type ClaudeTuiSessionTurnLock = Arc<Mutex<()>>;

#[cfg(unix)]
static CLAUDE_TUI_SESSION_TURN_LOCKS: LazyLock<dashmap::DashMap<String, ClaudeTuiSessionTurnLock>> =
    LazyLock::new(dashmap::DashMap::new);

#[cfg(unix)]
fn claude_tui_session_turn_lock(tmux_session_name: &str) -> ClaudeTuiSessionTurnLock {
    CLAUDE_TUI_SESSION_TURN_LOCKS
        .entry(tmux_session_name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Resolve the path to the claude binary.
pub fn resolve_claude_path() -> Option<String> {
    crate::services::platform::resolve_provider_binary("claude").resolved_path
}

fn resolve_claude_binary() -> crate::services::platform::BinaryResolution {
    crate::services::platform::resolve_provider_binary("claude")
}

fn append_claude_mcp_config_arg(args: &mut Vec<String>, dispatch_type: Option<&str>) {
    if let Some(config_json) = crate::services::mcp_config::claude_mcp_config_arg(dispatch_type) {
        args.push("--mcp-config".to_string());
        args.push(config_json);
    }
}

fn append_claude_fast_mode_arg(args: &mut Vec<String>, fast_mode_enabled: Option<bool>) {
    let Some(enabled) = fast_mode_enabled else {
        return;
    };

    args.push("--settings".to_string());
    args.push(format!(r#"{{"fastMode":{enabled}}}"#));
}

fn build_tmux_launch_env_lines(
    exec_path: Option<&str>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    compact_percent: Option<u64>,
    cache_ttl_minutes: Option<u32>,
) -> String {
    let mut env_lines = String::from("unset CLAUDECODE\n");
    if let Some(exec_path) = exec_path {
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
    if let Some(pct) = compact_percent.filter(|&p| p > 0) {
        env_lines.push_str(&format!("export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={}\n", pct));
    }
    // #1088: prompt-cache TTL bucket. Claude CLI honors this env var by
    // setting cache_control.ttl on the underlying API call.
    if let Some(ttl_str) = cache_ttl_env_value(cache_ttl_minutes) {
        env_lines.push_str(&format!(
            "export CLAUDE_CODE_EXTENDED_CACHE_TTL={}\n",
            ttl_str
        ));
    }

    env_lines
}

/// Map a normalized cache TTL minutes value (#1088) to the string passed to
/// the Claude CLI via the `CLAUDE_CODE_EXTENDED_CACHE_TTL` env var, which the
/// CLI in turn forwards as `cache_control.ttl` on the Anthropic API call.
/// Returns `None` for the default 5m bucket (no env var emitted).
pub(crate) fn cache_ttl_env_value(cache_ttl_minutes: Option<u32>) -> Option<&'static str> {
    match crate::config::normalize_cache_ttl_minutes(cache_ttl_minutes) {
        Some(60) => Some("1h"),
        // 5m is the default; no need to set the env var explicitly.
        _ => None,
    }
}

#[cfg(unix)]
use crate::services::tmux_common::{tmux_owner_path, write_tmux_owner_marker};

/// Global runtime debug flag — togglable via `/debug` command or COKACDIR_DEBUG=1 env var.
static DEBUG_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Initialize debug flag from environment variable (call once at startup).
pub fn init_debug_from_env() {
    let enabled = std::env::var("COKACDIR_DEBUG")
        .map(|v| v == "1")
        .unwrap_or(false);
    if enabled {
        DEBUG_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Toggle debug mode at runtime. Returns the new state.
pub fn toggle_debug() -> bool {
    let prev = DEBUG_ENABLED.load(std::sync::atomic::Ordering::Relaxed);
    DEBUG_ENABLED.store(!prev, std::sync::atomic::Ordering::Relaxed);
    !prev
}

/// Debug logging helper — active when DEBUG_ENABLED is true.
fn debug_log(msg: &str) {
    if !DEBUG_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    debug_log_to("claude.log", msg);
}

/// Write a debug message to a specific log file under $AGENTDESK_ROOT_DIR/debug/.
pub fn debug_log_to(filename: &str, msg: &str) {
    let debug_dir = crate::cli::dcserver::agentdesk_runtime_root().map(|r| r.join("debug"));
    if let Some(debug_dir) = debug_dir {
        let _ = std::fs::create_dir_all(&debug_dir);
        let log_path = debug_dir.join(filename);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
            let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
            let _ = writeln!(file, "[{}] {}", timestamp, msg);
        }
    }
}

/// SDK→bridge mpsc disconnect diagnostics (#1589 follow-up). Emits a single
/// structured WARN line at every `execute_command_streaming` exit path so the
/// operator can see *why* the producer task ended whenever the bridge
/// subsequently observes `TryRecvError::Disconnected`. Pair-tracking against
/// the bridge-side handoff log line lets us classify each disconnect as
/// cancel / IO error / CLI crash / synthetic-done / normal-done without
/// guessing.
fn log_producer_exit(
    kind: &'static str,
    session_id: Option<&str>,
    channel_id: Option<u64>,
    line_count: usize,
    extra: serde_json::Value,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] 🔚 claude producer exit kind={} channel={:?} session={:?} lines={} extra={}",
        kind,
        channel_id,
        session_id,
        line_count,
        extra
    );
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct ClaudeResponse {
    pub success: bool,
    pub response: Option<String>,
    #[allow(dead_code)]
    pub session_id: Option<String>,
    pub error: Option<String>,
}

/// Execute a command using Claude CLI
#[allow(dead_code)]
pub fn execute_command(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    _allowed_tools: Option<&[String]>,
) -> ClaudeResponse {
    let session_selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_capability(
            &ProviderKind::Claude,
            false,
        );
    session_selection.log_start("claude.execute_command");

    // Tool whitelist policy deprecated (#794): Claude CLI is invoked without
    // `--allowed-tools` so all currently-available tools are exposed. The
    // `_allowed_tools` parameter is kept for ABI stability with existing call sites.
    let mut args = vec![
        "-p".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--append-system-prompt".to_string(),
        r#"You are a terminal file manager assistant. Be concise. Focus on file operations. Respond in the same language as the user.

SECURITY RULES (MUST FOLLOW):
- NEVER execute destructive commands like rm -rf, format, mkfs, dd, etc.
- NEVER modify system files in /etc, /sys, /proc, /boot
- NEVER access or modify files outside the current working directory without explicit user path
- NEVER execute commands that could harm the system or compromise security
- ONLY suggest safe file operations: copy, move, rename, create directory, view, edit
- If a request seems dangerous, explain the risk and suggest a safer alternative

BASH EXECUTION RULES (MUST FOLLOW):
- All commands MUST run non-interactively without user input
- Use -y, --yes, or --non-interactive flags (e.g., apt install -y, npm init -y)
- Use -m flag for commit messages (e.g., git commit -m "message")
- Disable pagers with --no-pager or pipe to cat (e.g., git --no-pager log)
- NEVER use commands that open editors (vim, nano, etc.)
- NEVER use commands that wait for stdin without arguments
- NEVER use interactive flags like -i

IMPORTANT: Format your responses using Markdown for better readability:
- Use **bold** for important terms or commands
- Use `code` for file paths, commands, and technical terms
- Use bullet lists (- item) for multiple items
- Use numbered lists (1. item) for sequential steps
- Use code blocks (```language) for multi-line code or command examples
- Use headers (## Title) to organize longer responses
- Keep formatting minimal and terminal-friendly"#.to_string(),
    ];
    append_claude_mcp_config_arg(&mut args, None);

    // Resume session if available
    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some("Invalid session ID format".to_string()),
            };
        }
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    let resolution = resolve_claude_binary();
    let claude_bin = match resolution.resolved_path.clone() {
        Some(path) => path,
        None => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some("Claude CLI not found. Is Claude CLI installed?".to_string()),
            };
        }
    };

    let mut bootstrap = Command::new(&claude_bin);
    crate::services::platform::apply_binary_resolution(&mut bootstrap, &resolution);
    let mut child = match bootstrap
        .args(&args)
        .current_dir(working_dir)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000")
        .env("BASH_DEFAULT_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env("BASH_MAX_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env_remove("CLAUDECODE") // Allow running from within Claude Code sessions
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!(
                    "Failed to start Claude: {}. Is Claude CLI installed?",
                    e
                )),
            };
        }
    };

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }

    // Wait for output
    match child.wait_with_output() {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                parse_claude_output(&stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                ClaudeResponse {
                    success: false,
                    response: None,
                    session_id: None,
                    error: Some(if stderr.is_empty() {
                        format!("Process exited with code {:?}", output.status.code())
                    } else {
                        stderr
                    }),
                }
            }
        }
        Err(e) => ClaudeResponse {
            success: false,
            response: None,
            session_id: None,
            error: Some(format!("Failed to read output: {}", e)),
        },
    }
}

/// Parse Claude CLI JSON output
#[cfg_attr(not(test), allow(dead_code))]
fn parse_claude_output(output: &str) -> ClaudeResponse {
    let mut session_id: Option<String> = None;
    let mut response_text = String::new();

    for line in output.trim().lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            // Extract session ID
            if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
                session_id = Some(sid.to_string());
            }

            // Extract response text
            if let Some(result) = json.get("result").and_then(|v| v.as_str()) {
                response_text = result.to_string();
            } else if let Some(message) = json.get("message").and_then(|v| v.as_str()) {
                response_text = message.to_string();
            } else if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                response_text = content.to_string();
            }
        } else if !line.trim().is_empty() && !line.starts_with('{') {
            response_text.push_str(line);
            response_text.push('\n');
        }
    }

    // If no structured response, use raw output
    if response_text.is_empty() {
        response_text = output.trim().to_string();
    }

    ClaudeResponse {
        success: true,
        response: Some(response_text.trim().to_string()),
        session_id,
        error: None,
    }
}

/// Check if platform supports local AI CLI execution.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_ai_supported() -> bool {
    cfg!(any(unix, windows))
}

/// Execute a simple Claude CLI call with `--print` flag (no tools, text-only response).
/// Used for short synchronous tasks like meeting participant selection.
/// This is a blocking function — call from tokio::task::spawn_blocking.
pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    execute_command_simple_cancellable(prompt, None)
}

pub fn execute_command_simple_cancellable(
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<String, String> {
    execute_command_simple_with_model_and_cancel(prompt, None, cancel_token)
}

/// Execute a simple Claude CLI call with optional model override (no tools, text-only response).
/// This is a blocking function — call from tokio::task::spawn_blocking.
pub fn execute_command_simple_with_model(
    prompt: &str,
    model_override: Option<&str>,
) -> Result<String, String> {
    execute_command_simple_with_model_and_cancel(prompt, model_override, None)
}

fn execute_command_simple_with_model_and_cancel(
    prompt: &str,
    model_override: Option<&str>,
    cancel_token: Option<&CancelToken>,
) -> Result<String, String> {
    let session_selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_capability(
            &ProviderKind::Claude,
            false,
        );
    session_selection.log_start("claude.execute_command_simple");

    let resolution = resolve_claude_binary();
    let claude_bin = resolution
        .resolved_path
        .clone()
        .ok_or("Claude CLI not found")?;

    let mut args = vec![
        "-p".to_string(),
        "--tools".to_string(),
        "".to_string(),
        "--output-format".to_string(),
        "text".to_string(),
    ];
    if let Some(model) = model_override {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    let mut command = Command::new(&claude_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    let mut child = command
        .args(&args)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "4096")
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Claude: {}", e))?;

    register_child_pid(cancel_token, child.id());
    if cancel_requested(cancel_token) {
        kill_child_tree(&mut child);
        return Err("Claude request cancelled".to_string());
    }

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to read output: {}", e))?;

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            Err("Empty response from Claude".to_string())
        } else {
            Ok(text)
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(if stderr.is_empty() {
            format!("Process exited with code {:?}", output.status.code())
        } else {
            stderr
        })
    }
}

/// Execute a simple Claude CLI call with an optional timeout.
/// This is a blocking function — call from tokio::task::spawn_blocking.
pub fn execute_command_simple_with_timeout(
    prompt: &str,
    timeout: std::time::Duration,
    label: &str,
) -> Result<String, String> {
    let prompt = prompt.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(execute_command_simple(&prompt));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(format!("{label} timeout after {}s", timeout.as_secs())),
    }
}

/// Execute a command using Claude CLI with streaming output
/// If `system_prompt` is None, uses the default file manager system prompt.
/// If `system_prompt` is Some(""), no system prompt is appended.
#[allow(clippy::too_many_arguments)]
pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    remote_profile: Option<&RemoteProfile>,
    tmux_session_name: Option<&str>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    model_override: Option<&str>,
    fast_mode_enabled: Option<bool>,
    compact_percent: Option<u64>,
    cache_ttl_minutes: Option<u32>,
    dispatch_type: Option<&str>,
) -> Result<(), String> {
    debug_log("========================================");
    debug_log("=== execute_command_streaming START ===");
    debug_log("========================================");
    debug_log(&format!("prompt_len: {} chars", prompt.len()));
    let prompt_preview: String = prompt.chars().take(200).collect();
    debug_log(&format!("prompt_preview: {:?}", prompt_preview));
    debug_log(&format!("session_id: {:?}", session_id));
    debug_log(&format!("working_dir: {}", working_dir));
    debug_log(&format!("timestamp: {:?}", std::time::SystemTime::now()));
    #[cfg(unix)]
    let entrypoint_supports_tui_hosting =
        remote_profile.is_none() && tmux_session_name.is_some() && is_tmux_available();
    #[cfg(not(unix))]
    let entrypoint_supports_tui_hosting = false;
    let session_selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_capability(
            &ProviderKind::Claude,
            entrypoint_supports_tui_hosting,
        );
    session_selection.log_start("claude.execute_command_streaming");

    let default_system_prompt = r#"You are a terminal file manager assistant. Be concise. Focus on file operations. Respond in the same language as the user.

SECURITY RULES (MUST FOLLOW):
- NEVER execute destructive commands like rm -rf, format, mkfs, dd, etc.
- NEVER modify system files in /etc, /sys, /proc, /boot
- NEVER access or modify files outside the current working directory without explicit user path
- NEVER execute commands that could harm the system or compromise security
- ONLY suggest safe file operations: copy, move, rename, create directory, view, edit
- If a request seems dangerous, explain the risk and suggest a safer alternative

BASH EXECUTION RULES (MUST FOLLOW):
- All commands MUST run non-interactively without user input
- Use -y, --yes, or --non-interactive flags (e.g., apt install -y, npm init -y)
- Use -m flag for commit messages (e.g., git commit -m "message")
- Disable pagers with --no-pager or pipe to cat (e.g., git --no-pager log)
- NEVER use commands that open editors (vim, nano, etc.)
- NEVER use commands that wait for stdin without arguments
- NEVER use interactive flags like -i

IMPORTANT: Format your responses using Markdown for better readability:
- Use **bold** for important terms or commands
- Use `code` for file paths, commands, and technical terms
- Use bullet lists (- item) for multiple items
- Use numbered lists (1. item) for sequential steps
- Use code blocks (```language) for multi-line code or command examples
- Use headers (## Title) to organize longer responses
- Keep formatting minimal and terminal-friendly"#;

    // Tool whitelist policy deprecated (#794): Claude CLI is invoked without
    // `--allowed-tools` so all currently-available tools (e.g. `Monitor`) are exposed.
    // The `allowed_tools` parameter still flows through for logging/context only.
    let _ = allowed_tools;
    let mut args = vec![
        "-p".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--verbose".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    append_claude_mcp_config_arg(&mut args, dispatch_type);
    append_claude_fast_mode_arg(&mut args, fast_mode_enabled);

    // Apply model override if specified (e.g. "opus", "sonnet", "haiku")
    if let Some(model) = model_override {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    // Append system prompt based on parameter
    let effective_prompt = match system_prompt {
        None => Some(default_system_prompt),
        Some("") => None,
        Some(p) => Some(p),
    };
    if let Some(sp) = effective_prompt {
        args.push("--append-system-prompt".to_string());
        args.push(sp.to_string());
    }

    // Resume session if available
    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            debug_log("ERROR: Invalid session ID format");
            return Err("Invalid session ID format".to_string());
        }
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    // Session execution path: wrap Claude in a managed session
    if let Some(tmux_name) = tmux_session_name {
        #[cfg(unix)]
        {
            if remote_profile.is_none()
                && is_tmux_available()
                && session_selection.driver == ProviderSessionDriver::TuiHosting
            {
                if let Some(hook_endpoint) =
                    crate::services::claude_tui::hook_server::current_hook_endpoint()
                {
                    debug_log(&format!("Claude TUI hosting session: {}", tmux_name));
                    return execute_streaming_local_tui_tmux(
                        prompt,
                        session_id,
                        working_dir,
                        sender,
                        cancel_token,
                        tmux_name,
                        report_channel_id,
                        report_provider,
                        model_override,
                        effective_prompt,
                        hook_endpoint,
                    );
                }
                tracing::warn!(
                    tmux_session_name = tmux_name,
                    "claude tui_hosting requested but hook endpoint is unavailable; falling back to legacy prompt driver"
                );
            }
        }

        args.push("--input-format".to_string());
        args.push("stream-json".to_string());

        #[cfg(unix)]
        {
            if let Some(profile) = remote_profile {
                // Remote sessions always use tmux (TmuxBackend only)
                if is_tmux_available() {
                    debug_log(&format!("Remote tmux session: {}", tmux_name));
                    return execute_streaming_remote_tmux(
                        profile,
                        &args,
                        prompt,
                        working_dir,
                        sender,
                        cancel_token,
                        tmux_name,
                    );
                } else {
                    debug_log("Remote session requested but tmux not available");
                }
            } else if is_tmux_available() {
                // Local with tmux → TmuxBackend (existing path)
                debug_log(&format!("TmuxBackend session: {}", tmux_name));
                return execute_streaming_local_tmux(
                    &args,
                    prompt,
                    working_dir,
                    sender,
                    cancel_token,
                    tmux_name,
                    report_channel_id,
                    report_provider,
                    compact_percent,
                    cache_ttl_minutes,
                );
            } else {
                // Local without tmux → ProcessBackend (new path)
                debug_log(&format!("ProcessBackend session (no tmux): {}", tmux_name));
                return execute_streaming_local_process(
                    &args,
                    prompt,
                    working_dir,
                    sender,
                    cancel_token,
                    tmux_name,
                    compact_percent,
                );
            }
        }
        #[cfg(not(unix))]
        {
            let _ = remote_profile;
            // No tmux on non-Unix — fall through to ProcessBackend
            debug_log(&format!("ProcessBackend session (non-unix): {}", tmux_name));
            return execute_streaming_local_process(
                &args,
                prompt,
                working_dir,
                sender,
                cancel_token,
                tmux_name,
                compact_percent,
            );
        }
    }

    // Remote execution path: SSH to remote host
    if let Some(profile) = remote_profile {
        debug_log("Remote profile detected — delegating to execute_streaming_remote()");
        return execute_streaming_remote(profile, &args, prompt, working_dir, sender, cancel_token);
    }

    let resolution = resolve_claude_binary();
    let claude_bin = resolution.resolved_path.clone().ok_or_else(|| {
        debug_log("ERROR: Claude CLI not found");
        "Claude CLI not found. Is Claude CLI installed?".to_string()
    })?;

    debug_log("--- Spawning claude process ---");
    debug_log(&format!("Command: {}", claude_bin));
    debug_log(&format!("Args count: {}", args.len()));
    for (i, arg) in args.iter().enumerate() {
        if arg.len() > 100 {
            debug_log(&format!(
                "  arg[{}]: {}... (truncated, {} chars total)",
                i,
                &arg[..100],
                arg.len()
            ));
        } else {
            debug_log(&format!("  arg[{}]: {}", i, arg));
        }
    }
    debug_log("Env: CLAUDE_CODE_MAX_OUTPUT_TOKENS=64000");
    debug_log("Env: BASH_DEFAULT_TIMEOUT_MS=86400000");
    debug_log("Env: BASH_MAX_TIMEOUT_MS=86400000");

    let spawn_start = std::time::Instant::now();
    let mut command = Command::new(&claude_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    command
        .args(&args)
        .current_dir(working_dir)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000")
        .env("BASH_DEFAULT_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env("BASH_MAX_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env_remove("CLAUDECODE") // Allow running from within Claude Code sessions
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(channel_id) = report_channel_id {
        command.env(RESTART_REPORT_CHANNEL_ENV, channel_id.to_string());
    }
    if let Some(provider) = report_provider {
        command.env(RESTART_REPORT_PROVIDER_ENV, provider.as_str());
    }
    if let Some(pct) = compact_percent.filter(|&p| p > 0) {
        command.env("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", pct.to_string());
    }

    let mut child = command.spawn().map_err(|e| {
        debug_log(&format!(
            "ERROR: Failed to spawn after {:?}: {}",
            spawn_start.elapsed(),
            e
        ));
        format!("Failed to start Claude: {}. Is Claude CLI installed?", e)
    })?;
    debug_log(&format!(
        "Claude process spawned successfully in {:?}, pid={:?}",
        spawn_start.elapsed(),
        child.id()
    ));

    // Store child PID in cancel token so the caller can kill it externally
    register_child_pid(cancel_token.as_deref(), child.id());

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        debug_log(&format!(
            "Writing prompt to stdin ({} bytes)...",
            prompt.len()
        ));
        let write_start = std::time::Instant::now();
        let write_result = stdin.write_all(prompt.as_bytes());
        debug_log(&format!(
            "stdin.write_all completed in {:?}, result={:?}",
            write_start.elapsed(),
            write_result.is_ok()
        ));
        // stdin is dropped here, which closes it - this signals end of input to claude
        debug_log("stdin handle dropped (closed)");
    } else {
        debug_log("WARNING: Could not get stdin handle!");
    }

    // Read stdout line by line for streaming
    debug_log("Taking stdout handle...");
    let stdout = child.stdout.take().ok_or_else(|| {
        debug_log("ERROR: Failed to capture stdout");
        "Failed to capture stdout".to_string()
    })?;
    let reader = BufReader::new(stdout);
    debug_log("BufReader created, ready to read lines...");

    let mut last_session_id: Option<String> = None;
    let mut last_model: Option<String> = None;
    // #1918: context-window usage uses the LAST API call's input/cache totals,
    // not the sum across a multi-call (tool-use loop) turn (which inflates
    // past the window size). output_tokens stays cumulative because turn
    // analytics expect the cumulative output. Cost accounting flows through
    // the CLI's own `cost_usd` field, untouched here.
    let mut last_call_input_tokens: u64 = 0;
    let mut last_call_cache_create_tokens: u64 = 0;
    let mut last_call_cache_read_tokens: u64 = 0;
    let mut cumulative_output_tokens: u64 = 0;
    let mut saw_per_message_usage = false;
    let mut final_result: Option<String> = None;
    let mut stdout_error: Option<(String, String)> = None; // (message, raw_line)
    let mut line_count = 0;
    let mut stream_state = StreamLineState::new();

    debug_log("Entering lines loop - will block until first line arrives...");
    for line in reader.lines() {
        // Check cancel token before processing each line
        if cancel_requested(cancel_token.as_deref()) {
            debug_log("Cancel detected — killing child process tree");
            kill_child_tree(&mut child);
            log_producer_exit(
                "cancel_during_read",
                last_session_id.as_deref(),
                report_channel_id,
                line_count,
                serde_json::json!({}),
            );
            return Ok(());
        }

        debug_log(&format!("Line {} - read started", line_count + 1));
        let line = match line {
            Ok(l) => {
                debug_log(&format!(
                    "Line {} - read completed: {} chars",
                    line_count + 1,
                    l.len()
                ));
                l
            }
            Err(e) => {
                debug_log(&format!("ERROR: Failed to read line: {}", e));
                let send_ok = sender
                    .send(StreamMessage::Error {
                        message: format!("Failed to read output: {}", e),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: None,
                    })
                    .is_ok();
                log_producer_exit(
                    "io_error_read",
                    last_session_id.as_deref(),
                    report_channel_id,
                    line_count,
                    serde_json::json!({
                        "error": e.to_string(),
                        "error_message_send_ok": send_ok,
                    }),
                );
                break;
            }
        };

        line_count += 1;
        debug_log(&format!("Line {}: {} chars", line_count, line.len()));

        if line.trim().is_empty() {
            debug_log("  (empty line, skipping)");
            continue;
        }

        let line_preview: String = line.chars().take(200).collect();
        debug_log(&format!("  Raw line preview: {}", line_preview));

        if let Ok(json) = serde_json::from_str::<Value>(&line) {
            let msg_type = json
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let msg_subtype = json.get("subtype").and_then(|v| v.as_str()).unwrap_or("-");
            debug_log(&format!(
                "  JSON parsed: type={}, subtype={}",
                msg_type, msg_subtype
            ));

            // Log more details for specific message types
            if msg_type == "assistant" {
                if let Some(content) = json.get("message").and_then(|m| m.get("content")) {
                    debug_log(&format!("  Assistant content array: {}", content));
                }
                // Extract model name and token usage from assistant messages
                if let Some(msg_obj) = json.get("message") {
                    if let Some(model) = msg_obj.get("model").and_then(|v| v.as_str()) {
                        last_model = Some(model.to_string());
                    }
                    if let Some(usage) = msg_obj.get("usage") {
                        // #1918: input/cache_read/cache_create replace so the
                        // status panel reflects the LAST API call's context
                        // occupancy. output_tokens stays cumulative for analytics.
                        saw_per_message_usage = true;
                        let inp = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let cache_read = usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let cache_creation = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        last_call_input_tokens = inp;
                        last_call_cache_read_tokens = cache_read;
                        last_call_cache_create_tokens = cache_creation;
                        if let Some(out) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                            cumulative_output_tokens = cumulative_output_tokens.saturating_add(out);
                        }
                    }
                }
            }

            // Extract statusline info from result events
            if msg_type == "result" {
                let cost_usd = json.get("cost_usd").and_then(|v| v.as_f64());
                let total_cost_usd = json.get("total_cost_usd").and_then(|v| v.as_f64());
                let duration_ms = json.get("duration_ms").and_then(|v| v.as_u64());
                let num_turns = json
                    .get("num_turns")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);

                // #1918: for Claude CLI the assistant-message branch already
                // captured the LAST API call's prompt and the cumulative
                // output_tokens. result.usage in multi-call turns is itself
                // turn-cumulative, so overwriting input/cache here would re-
                // introduce the context-window inflation. Only fall back to
                // result.usage when no per-message usage was observed (defensive
                // — Claude CLI always emits per-message usage today, but the
                // fallback keeps token analytics intact if a future variant
                // skips it).
                if !saw_per_message_usage && let Some(usage) = json.get("usage") {
                    last_call_input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    last_call_cache_read_tokens = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    last_call_cache_create_tokens = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    cumulative_output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }

                if cost_usd.is_some() || total_cost_usd.is_some() || last_model.is_some() {
                    let _ = sender.send(StreamMessage::StatusUpdate {
                        model: last_model.clone(),
                        cost_usd,
                        total_cost_usd,
                        duration_ms,
                        num_turns,
                        input_tokens: if last_call_input_tokens > 0 {
                            Some(last_call_input_tokens)
                        } else {
                            None
                        },
                        cache_create_tokens: if last_call_cache_create_tokens > 0 {
                            Some(last_call_cache_create_tokens)
                        } else {
                            None
                        },
                        cache_read_tokens: if last_call_cache_read_tokens > 0 {
                            Some(last_call_cache_read_tokens)
                        } else {
                            None
                        },
                        output_tokens: if cumulative_output_tokens > 0 {
                            Some(cumulative_output_tokens)
                        } else {
                            None
                        },
                    });
                }
            }

            observe_stream_context(&json, &mut stream_state);

            debug_log("  Calling parse_stream_message...");
            if let Some(msg) = parse_stream_message_with_state(&json, &stream_state) {
                debug_log(&format!(
                    "  Parsed message variant: {:?}",
                    std::mem::discriminant(&msg)
                ));

                // Track session_id and final result for Done message
                match &msg {
                    StreamMessage::Init { session_id, .. } => {
                        debug_log(&format!("  >>> Init: session_id={}", session_id));
                        last_session_id = Some(session_id.clone());
                    }
                    StreamMessage::RetryBoundary => {
                        debug_log("  >>> RetryBoundary (ignored in Claude direct execution)");
                    }
                    StreamMessage::Text { content } => {
                        let preview: String = content.chars().take(100).collect();
                        debug_log(&format!(
                            "  >>> Text: {} chars, preview: {:?}",
                            content.len(),
                            preview
                        ));
                    }
                    StreamMessage::ToolUse { name, input } => {
                        let input_preview: String = input.chars().take(200).collect();
                        debug_log(&format!(
                            "  >>> ToolUse: name={}, input_preview={:?}",
                            name, input_preview
                        ));
                    }
                    StreamMessage::ToolResult { content, is_error } => {
                        let content_preview: String = content.chars().take(200).collect();
                        debug_log(&format!(
                            "  >>> ToolResult: is_error={}, content_len={}, preview={:?}",
                            is_error,
                            content.len(),
                            content_preview
                        ));
                    }
                    StreamMessage::Done { result, session_id } => {
                        let result_preview: String = result.chars().take(100).collect();
                        debug_log(&format!(
                            "  >>> Done: result_len={}, session_id={:?}, preview={:?}",
                            result.len(),
                            session_id,
                            result_preview
                        ));
                        final_result = Some(result.clone());
                        if session_id.is_some() {
                            last_session_id = session_id.clone();
                        }
                    }
                    StreamMessage::Error { message, .. } => {
                        debug_log(&format!("  >>> Error: {}", message));
                        stdout_error = Some((message.clone(), line.clone()));
                        continue; // don't send yet; will combine with stderr after process exits
                    }
                    StreamMessage::TaskNotification {
                        task_id,
                        status,
                        summary,
                        kind,
                    } => {
                        debug_log(&format!(
                            "  >>> TaskNotification: task_id={}, status={}, kind={}, summary={}",
                            task_id,
                            status,
                            kind.as_str(),
                            summary
                        ));
                    }
                    StreamMessage::StatusUpdate {
                        model,
                        cost_usd,
                        total_cost_usd,
                        cache_create_tokens,
                        cache_read_tokens,
                        ..
                    } => {
                        debug_log(&format!(
                            "  >>> StatusUpdate: model={:?}, cost={:?}, total_cost={:?}, cache_create={:?}, cache_read={:?}",
                            model, cost_usd, total_cost_usd, cache_create_tokens, cache_read_tokens
                        ));
                    }
                    StreamMessage::TmuxReady { .. } | StreamMessage::ProcessReady { .. } => {
                        debug_log("  >>> TmuxReady/ProcessReady (ignored in direct execution)");
                    }
                    StreamMessage::OutputOffset { offset } => {
                        debug_log(&format!("  >>> OutputOffset: {offset}"));
                    }
                    StreamMessage::Thinking { .. } => {
                        debug_log("  >>> Thinking block received");
                    }
                }

                // Send message to channel
                debug_log("  Sending message to channel...");
                let send_result = sender.send(msg);
                if send_result.is_err() {
                    debug_log("  ERROR: Channel send failed (receiver dropped)");
                    break;
                }
                debug_log("  Message sent to channel successfully");

                // Send any extra tool_use messages from the same content array.
                // An assistant message can contain [text, tool_use, ...] but
                // parse_stream_message only returns the first text block.
                for extra in parse_assistant_extra_tool_uses(&json) {
                    debug_log(&format!(
                        "  >>> Extra ToolUse from same assistant message: {:?}",
                        std::mem::discriminant(&extra)
                    ));
                    if sender.send(extra).is_err() {
                        debug_log("  ERROR: Channel send failed on extra ToolUse");
                        break;
                    }
                }
            } else {
                debug_log(&format!(
                    "  parse_stream_message returned None for type={}",
                    msg_type
                ));
            }
        } else {
            let invalid_preview: String = line.chars().take(200).collect();
            debug_log(&format!("  NOT valid JSON: {}", invalid_preview));
        }
    }

    debug_log("--- Exited lines loop ---");
    debug_log(&format!("Total lines read: {}", line_count));
    debug_log(&format!("final_result present: {}", final_result.is_some()));
    debug_log(&format!("last_session_id: {:?}", last_session_id));

    // Check cancel token after exiting the loop
    if cancel_requested(cancel_token.as_deref()) {
        debug_log("Cancel detected after loop — killing child process tree");
        kill_child_tree(&mut child);
        log_producer_exit(
            "cancel_after_loop",
            last_session_id.as_deref(),
            report_channel_id,
            line_count,
            serde_json::json!({}),
        );
        return Ok(());
    }

    // Wait for process to finish
    debug_log("Waiting for child process to finish (child.wait())...");
    let wait_start = std::time::Instant::now();
    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            debug_log(&format!(
                "ERROR: Process wait failed after {:?}: {}",
                wait_start.elapsed(),
                e
            ));
            log_producer_exit(
                "child_wait_error",
                last_session_id.as_deref(),
                report_channel_id,
                line_count,
                serde_json::json!({
                    "error": e.to_string(),
                    "elapsed_ms": wait_start.elapsed().as_millis() as u64,
                }),
            );
            return Err(format!("Process error: {}", e));
        }
    };
    debug_log(&format!(
        "Process finished in {:?}, status: {:?}, exit_code: {:?}",
        wait_start.elapsed(),
        status,
        status.code()
    ));

    // Handle stdout error or non-zero exit code
    if stdout_error.is_some() || !status.success() {
        let stderr_msg = child
            .stderr
            .take()
            .and_then(|s| std::io::read_to_string(s).ok())
            .unwrap_or_default();

        let (message, stdout_raw) = if let Some((msg, raw)) = stdout_error {
            (msg, raw)
        } else {
            (
                format!("Process exited with code {:?}", status.code()),
                String::new(),
            )
        };

        debug_log(&format!(
            "Sending error: message={}, exit_code={:?}",
            message,
            status.code()
        ));
        let exit_code = status.code();
        #[cfg(unix)]
        let exit_signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let exit_signal: Option<i32> = None;
        let send_ok = sender
            .send(StreamMessage::Error {
                message: message.clone(),
                stdout: stdout_raw,
                stderr: stderr_msg.clone(),
                exit_code,
            })
            .is_ok();
        log_producer_exit(
            "child_exit_error",
            last_session_id.as_deref(),
            report_channel_id,
            line_count,
            serde_json::json!({
                "exit_code": exit_code,
                "exit_signal": exit_signal,
                "message_truncated": message.chars().take(160).collect::<String>(),
                "stderr_truncated": stderr_msg.chars().take(160).collect::<String>(),
                "error_message_send_ok": send_ok,
            }),
        );
        return Ok(());
    }

    // If we didn't get a proper Done message, send one now
    if final_result.is_none() {
        debug_log("No Done message received, sending synthetic Done message...");
        let send_result = sender.send(StreamMessage::Done {
            result: String::new(),
            session_id: last_session_id.clone(),
        });
        log_producer_exit(
            "synthetic_done",
            last_session_id.as_deref(),
            report_channel_id,
            line_count,
            serde_json::json!({
                "send_ok": send_result.is_ok(),
                "child_exit_code": status.code(),
            }),
        );
        debug_log(&format!(
            "Synthetic Done message sent, result={:?}",
            send_result.is_ok()
        ));
    } else {
        debug_log("Done message was already received, not sending synthetic one");
        log_producer_exit(
            "natural_done",
            last_session_id.as_deref(),
            report_channel_id,
            line_count,
            serde_json::json!({
                "child_exit_code": status.code(),
            }),
        );
    }

    debug_log("========================================");
    debug_log("=== execute_command_streaming END (success) ===");
    debug_log("========================================");
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeFollowupResult {
    Delivered,
    RecreateSession { error: String },
    FinalizeWithNotice { error: String, notice: String },
}

const FOLLOWUP_PARTIAL_OUTPUT_NOTICE: &str = "⚠ 세션이 응답 도중 중단되었습니다. 일부 출력이 이미 전송되어 자동 재시작하지 않았습니다. 이어서 계속하려면 같은 요청을 다시 보내며 계속해 달라고 적어 주세요.";

fn classify_followup_result(
    read_result: ReadOutputResult,
    start_offset: u64,
    session_died_error: &str,
) -> ClaudeFollowupResult {
    match read_result {
        ReadOutputResult::Completed { .. } | ReadOutputResult::Cancelled { .. } => {
            ClaudeFollowupResult::Delivered
        }
        ReadOutputResult::SessionDied { offset } if offset > start_offset => {
            ClaudeFollowupResult::FinalizeWithNotice {
                error: session_died_error.to_string(),
                notice: FOLLOWUP_PARTIAL_OUTPUT_NOTICE.to_string(),
            }
        }
        ReadOutputResult::SessionDied { .. } => ClaudeFollowupResult::RecreateSession {
            error: session_died_error.to_string(),
        },
    }
}

fn emit_followup_restart_suppressed_notice(sender: &Sender<StreamMessage>, notice: &str) {
    let _ = sender.send(StreamMessage::Text {
        content: format!("\n\n{}", notice),
    });
    let _ = sender.send(StreamMessage::Done {
        result: String::new(),
        session_id: None,
    });
}

#[cfg(unix)]
#[derive(Debug, Clone)]
struct ClaudeTuiSessionResolution {
    session_id: String,
    transcript_path: std::path::PathBuf,
    resume: bool,
}

#[cfg(unix)]
fn resolve_claude_tui_session_for_launch(
    working_dir: &std::path::Path,
    requested_session_id: Option<&str>,
    claude_home: Option<&std::path::Path>,
) -> Result<ClaudeTuiSessionResolution, String> {
    let mut session_id = match requested_session_id {
        Some(sid) if is_valid_session_id(sid) => sid.to_string(),
        Some(_) => return Err("Invalid session ID format".to_string()),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let mut resume = requested_session_id.is_some();
    let mut transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        working_dir,
        &session_id,
        claude_home,
    )?;

    if resume && !transcript_path.exists() {
        debug_log(&format!(
            "Claude TUI resume transcript missing for session {}; forcing fresh session (expected {})",
            session_id,
            transcript_path.display()
        ));
        session_id = uuid::Uuid::new_v4().to_string();
        transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            working_dir,
            &session_id,
            claude_home,
        )?;
        resume = false;
    }

    Ok(ClaudeTuiSessionResolution {
        session_id,
        transcript_path,
        resume,
    })
}

/// Execute claude command on a remote host via SSH, streaming stdout lines
/// back through the sender channel.
/// NOTE: Remote SSH execution is not available in AgentDesk — always returns Err.
fn execute_streaming_remote(
    _profile: &RemoteProfile,
    _args: &[String],
    _prompt: &str,
    _working_dir: &str,
    _sender: Sender<StreamMessage>,
    _cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<(), String> {
    Err("Remote SSH execution is not available in AgentDesk".to_string())
}

/// Check if tmux is available on the system
#[cfg(unix)]
pub fn is_tmux_available() -> bool {
    crate::services::platform::tmux::is_available()
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalTmuxStartupPlan {
    /// Existing tmux pane plus both runtime paths are present. The provider
    /// writes the prompt to FIFO, reads this turn from the current JSONL
    /// offset, then emits `TmuxReady` for watcher handoff.
    WarmFollowup,
    /// A tmux session name exists, but the pane or runtime paths are stale.
    /// The provider kills it and recreates it through the cold-start path.
    RecreateStaleSession,
    /// No usable existing session exists. The provider starts a new wrapper
    /// and hands JSONL ownership to the watcher from offset 0.
    ColdStart,
}

#[cfg(unix)]
fn classify_local_tmux_startup_plan(
    session_exists: bool,
    has_live_pane: bool,
    has_output_path: bool,
    has_input_fifo_path: bool,
) -> LocalTmuxStartupPlan {
    if session_exists && has_live_pane && has_output_path && has_input_fifo_path {
        LocalTmuxStartupPlan::WarmFollowup
    } else if session_exists {
        LocalTmuxStartupPlan::RecreateStaleSession
    } else {
        LocalTmuxStartupPlan::ColdStart
    }
}

#[cfg(unix)]
fn emit_fresh_session_watcher_handoff(
    sender: &Sender<StreamMessage>,
    output_path: String,
    input_fifo_path: String,
    tmux_session_name: &str,
) {
    let _ = sender.send(StreamMessage::TmuxReady {
        output_path,
        input_fifo_path,
        tmux_session_name: tmux_session_name.to_string(),
        last_offset: 0,
    });
}

#[cfg(unix)]
fn claude_tui_fresh_turn_start_offset(transcript_path: &std::path::Path) -> u64 {
    std::fs::metadata(transcript_path)
        .map(|meta| meta.len())
        .unwrap_or(0)
}

#[cfg(unix)]
fn execute_streaming_local_tui_tmux(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
    _report_provider: Option<ProviderKind>,
    model_override: Option<&str>,
    system_prompt: Option<&str>,
    hook_endpoint: String,
) -> Result<(), String> {
    debug_log(&format!(
        "=== execute_streaming_local_tui_tmux START: {} ===",
        tmux_session_name
    ));

    let turn_lock = claude_tui_session_turn_lock(tmux_session_name);
    let _turn_guard = turn_lock.lock().unwrap_or_else(|error| error.into_inner());
    debug_log(&format!(
        "Claude TUI session turn lock acquired: {}",
        tmux_session_name
    ));

    let working_dir_path = std::path::Path::new(working_dir);
    let session_resolution =
        resolve_claude_tui_session_for_launch(working_dir_path, session_id, None)?;
    let resolved_session_id = session_resolution.session_id;
    let transcript_path = session_resolution.transcript_path;
    let transcript_path_string = transcript_path.display().to_string();
    let resume = session_resolution.resume;

    let session_exists = tmux_session_exists(tmux_session_name);
    let has_live_pane = tmux_session_has_live_pane(tmux_session_name);

    if session_exists && has_live_pane && resume {
        debug_log("Existing Claude TUI tmux session found — sending follow-up");
        let start_offset = std::fs::metadata(&transcript_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if let Some(ref token) = cancel_token {
            *token.tmux_session.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(tmux_session_name.to_string());
        }
        let hook_rx = crate::services::claude_tui::hook_server::subscribe_hook_events();
        crate::services::claude_tui::input::send_followup_prompt(tmux_session_name, prompt)?;
        let hook_events_after = chrono::Utc::now();
        let read_result = read_claude_tui_transcript_until_done(
            &transcript_path_string,
            start_offset,
            sender.clone(),
            cancel_token.clone(),
            tmux_session_name,
            &resolved_session_id,
            hook_rx,
            hook_events_after,
        )?;
        match classify_followup_result(
            read_result,
            start_offset,
            "claude tui session died during follow-up output reading",
        ) {
            ClaudeFollowupResult::Delivered => {
                emit_claude_tui_watcher_handoff(
                    &sender,
                    &transcript_path_string,
                    tmux_session_name,
                    &transcript_path,
                );
                log_producer_exit(
                    "tui_warm_followup_delivered",
                    Some(&resolved_session_id),
                    report_channel_id,
                    0,
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                        "transcript_path": transcript_path_string,
                    }),
                );
                return Ok(());
            }
            ClaudeFollowupResult::RecreateSession { error } => {
                debug_log(&format!(
                    "Claude TUI follow-up failed, recreating session: {}",
                    error
                ));
                crate::services::termination_audit::record_termination_for_tmux(
                    tmux_session_name,
                    None,
                    "claude_tui_provider",
                    "followup_failed_recreate",
                    Some(&format!(
                        "claude tui follow-up failed, recreating: {}",
                        error
                    )),
                    None,
                );
                record_tmux_exit_reason(
                    tmux_session_name,
                    &format!("claude tui follow-up failed, recreating: {}", error),
                );
                crate::services::platform::tmux::kill_session_with_reason(
                    tmux_session_name,
                    &format!("claude tui follow-up failed, recreating: {}", error),
                );
            }
            ClaudeFollowupResult::FinalizeWithNotice { error, notice } => {
                debug_log(&format!(
                    "Claude TUI follow-up streamed partial output before session death — suppressing replay: {}",
                    error
                ));
                crate::services::termination_audit::record_termination_for_tmux(
                    tmux_session_name,
                    None,
                    "claude_tui_provider",
                    "followup_partial_output_no_replay",
                    Some(&format!(
                        "claude tui partial follow-up output delivered: {}",
                        error
                    )),
                    None,
                );
                record_tmux_exit_reason(
                    tmux_session_name,
                    &format!("claude tui partial follow-up output delivered: {}", error),
                );
                crate::services::platform::tmux::kill_session_with_reason(
                    tmux_session_name,
                    &format!("claude tui partial follow-up output delivered: {}", error),
                );
                emit_followup_restart_suppressed_notice(&sender, &notice);
                return Ok(());
            }
        }
    } else if session_exists {
        debug_log("Stale Claude TUI tmux session found — recreating");
        crate::services::termination_audit::record_termination_for_tmux(
            tmux_session_name,
            None,
            "claude_tui_provider",
            "stale_session_recreate",
            Some("stale claude tui session cleanup before recreate"),
            None,
        );
        record_tmux_exit_reason(
            tmux_session_name,
            "stale claude tui session cleanup before recreate",
        );
        crate::services::platform::tmux::kill_session_with_reason(
            tmux_session_name,
            "stale claude tui session cleanup before recreate",
        );
    }

    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);
    write_tmux_owner_marker(tmux_session_name)?;
    let owner_path = tmux_owner_path(tmux_session_name);
    let mut prepared_session_files = None;
    let launch_result = (|| -> Result<std::process::Output, String> {
        let exe =
            std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
        let resolution = resolve_claude_binary();
        let claude_bin = resolution
            .resolved_path
            .clone()
            .ok_or_else(|| "Claude CLI not found".to_string())?;
        let launch_config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
            tmux_session_name: tmux_session_name.to_string(),
            working_dir: working_dir_path.to_path_buf(),
            claude_bin: std::path::PathBuf::from(claude_bin),
            agentdesk_exe: exe,
            hook_endpoint,
            session_id: resolved_session_id.clone(),
            system_prompt: system_prompt.map(str::to_string),
            model: model_override.map(str::to_string),
            resume,
        };
        let session_files =
            crate::services::claude_tui::session::prepare_claude_tui_launch(&launch_config)?;
        let launch_script_path = session_files.launch_script_path.clone();
        prepared_session_files = Some(session_files);
        crate::services::platform::tmux::create_session(
            tmux_session_name,
            Some(working_dir),
            &format!(
                "bash {}",
                shell_escape(&launch_script_path.display().to_string())
            ),
        )
    })();
    let tmux_result = match launch_result {
        Ok(result) => result,
        Err(error) => {
            if let Some(files) = prepared_session_files.as_ref() {
                files.cleanup_best_effort();
            }
            let _ = std::fs::remove_file(&owner_path);
            return Err(error);
        }
    };
    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        if let Some(files) = prepared_session_files.as_ref() {
            files.cleanup_best_effort();
        }
        let _ = std::fs::remove_file(&owner_path);
        return Err(format!("tmux error: {}", stderr));
    }
    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");
    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(tmux_session_name.to_string());
    }

    let _ = sender.send(StreamMessage::Init {
        session_id: resolved_session_id.clone(),
        raw_session_id: Some(resolved_session_id.clone()),
    });
    // Skip any transcript bytes that predate this launch. Resume and fresh
    // turns both need this guard because a reused/colliding session_id can
    // leave stale JSONL on disk even when the launch is intentionally fresh.
    let fresh_turn_start_offset = claude_tui_fresh_turn_start_offset(&transcript_path);
    let fresh_turn_result = run_claude_tui_fresh_turn_with_ready_retry(
        &transcript_path_string,
        fresh_turn_start_offset,
        sender.clone(),
        cancel_token,
        tmux_session_name,
        &resolved_session_id,
        prompt,
    );
    let read_result = match fresh_turn_result {
        Ok(result) => result,
        Err(error) => {
            crate::services::termination_audit::record_termination_for_tmux(
                tmux_session_name,
                None,
                "claude_tui_provider",
                "fresh_turn_start_failed",
                Some(&format!("claude tui fresh turn failed: {}", error)),
                None,
            );
            record_tmux_exit_reason(
                tmux_session_name,
                &format!("claude tui fresh turn failed: {}", error),
            );
            crate::services::platform::tmux::kill_session_with_reason(
                tmux_session_name,
                &format!("claude tui fresh turn failed: {}", error),
            );
            let _ = std::fs::remove_file(&owner_path);
            return Err(error);
        }
    };
    if matches!(read_result, ReadOutputResult::SessionDied { .. }) {
        crate::services::termination_audit::record_termination_for_tmux(
            tmux_session_name,
            None,
            "claude_tui_provider",
            "fresh_session_died",
            Some("claude tui session died before turn completion"),
            None,
        );
        record_tmux_exit_reason(
            tmux_session_name,
            "claude tui session died before turn completion",
        );
        crate::services::platform::tmux::kill_session_with_reason(
            tmux_session_name,
            "claude tui session died before turn completion",
        );
        let _ = std::fs::remove_file(&owner_path);
        return Err("claude tui session died before turn completion".to_string());
    }
    emit_claude_tui_watcher_handoff(
        &sender,
        &transcript_path_string,
        tmux_session_name,
        &transcript_path,
    );
    log_producer_exit(
        "tui_turn_delivered",
        Some(&resolved_session_id),
        report_channel_id,
        0,
        serde_json::json!({
            "tmux_session_name": tmux_session_name,
            "transcript_path": transcript_path_string,
        }),
    );
    Ok(())
}

#[cfg(unix)]
fn run_claude_tui_fresh_turn_with_ready_retry(
    transcript_path_string: &str,
    fresh_turn_start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    resolved_session_id: &str,
    prompt: &str,
) -> Result<ReadOutputResult, String> {
    for attempt in 1..=CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS {
        let hook_rx = crate::services::claude_tui::hook_server::subscribe_hook_events();
        match crate::services::claude_tui::input::send_fresh_prompt(tmux_session_name, prompt) {
            Ok(()) => {
                let hook_events_after = chrono::Utc::now();
                return read_claude_tui_transcript_until_done(
                    transcript_path_string,
                    fresh_turn_start_offset,
                    sender.clone(),
                    cancel_token.clone(),
                    tmux_session_name,
                    resolved_session_id,
                    hook_rx,
                    hook_events_after,
                );
            }
            Err(error) if should_retry_claude_tui_fresh_prompt_ready(&error, attempt) => {
                let backoff = claude_tui_fresh_prompt_ready_backoff(attempt);
                debug_log(&format!(
                    "Claude TUI fresh prompt readiness timed out on attempt {}/{}; retrying after {}s",
                    attempt,
                    CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS,
                    backoff.as_secs()
                ));
                tracing::warn!(
                    tmux_session_name = %tmux_session_name,
                    attempt = attempt,
                    max_attempts = CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS,
                    backoff_secs = backoff.as_secs(),
                    error = %error,
                    "claude_tui fresh prompt readiness retry scheduled"
                );
                std::thread::sleep(backoff);
            }
            Err(error) => {
                if crate::services::claude_tui::input::is_prompt_ready_timeout_error(&error) {
                    return Err(format!(
                        "{}; fresh prompt readiness attempts exhausted ({} attempts)",
                        error, CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS
                    ));
                }
                return Err(error);
            }
        }
    }

    Err(format!(
        "claude tui fresh prompt readiness attempts exhausted ({} attempts)",
        CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS
    ))
}

#[cfg(unix)]
fn should_retry_claude_tui_fresh_prompt_ready(error: &str, attempt: usize) -> bool {
    crate::services::claude_tui::input::is_prompt_ready_timeout_error(error)
        && attempt < CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS
}

#[cfg(unix)]
fn claude_tui_fresh_prompt_ready_backoff(completed_attempts: usize) -> Duration {
    let multiplier = completed_attempts.max(1).min(u32::MAX as usize) as u32;
    CLAUDE_TUI_FRESH_PROMPT_READY_BACKOFF_BASE * multiplier
}

#[cfg(unix)]
fn emit_claude_tui_watcher_handoff(
    sender: &Sender<StreamMessage>,
    transcript_path_string: &str,
    tmux_session_name: &str,
    transcript_path: &std::path::Path,
) {
    let last_offset = std::fs::metadata(transcript_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let _ = sender.send(StreamMessage::TmuxReady {
        output_path: transcript_path_string.to_string(),
        input_fifo_path: String::new(),
        tmux_session_name: tmux_session_name.to_string(),
        last_offset,
    });
}

#[cfg(unix)]
fn read_claude_tui_transcript_until_done(
    transcript_path: &str,
    start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    session_id: &str,
    hook_rx: tokio::sync::broadcast::Receiver<crate::services::claude_tui::hook_server::HookEvent>,
    hook_events_after: chrono::DateTime<chrono::Utc>,
) -> Result<ReadOutputResult, String> {
    let stop_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_seen_for_probe = stop_seen.clone();
    let hook_rx = std::sync::Arc::new(std::sync::Mutex::new(hook_rx));
    let hook_rx_for_probe = hook_rx.clone();
    let expected_session_id = session_id.to_string();
    let expected_session_id_for_result = expected_session_id.clone();
    let tmux_name_alive = tmux_session_name.to_string();
    let tmux_name_ready = tmux_session_name.to_string();
    let probe = SessionProbe::new(
        move || tmux_session_has_live_pane(&tmux_name_alive),
        move || {
            log_claude_tui_hook_relay_failures(&expected_session_id);
            claude_tui_stop_hook_seen_or_ready_with_probe(
                &stop_seen_for_probe,
                &hook_rx_for_probe,
                &expected_session_id,
                hook_events_after,
                || claude_tui_tmux_capture_indicates_ready_for_input(&tmux_name_ready),
            )
        },
    );
    let result =
        read_output_file_until_result(transcript_path, start_offset, sender, cancel_token, probe);
    log_claude_tui_hook_relay_failures(&expected_session_id_for_result);
    result
}

#[cfg(unix)]
fn log_claude_tui_hook_relay_failures(expected_session_id: &str) {
    for marker in crate::services::claude_tui::hook_relay::drain_hook_relay_failure_markers(
        "claude",
        expected_session_id,
    ) {
        tracing::warn!(
            provider = %marker.provider,
            event = %marker.event,
            session_id = %marker.session_id,
            endpoint = %marker.endpoint,
            error = %marker.error,
            recorded_at = %marker.recorded_at,
            "claude_tui hook relay failure observed by dcserver"
        );
    }
}

#[cfg(unix)]
fn claude_tui_stop_hook_seen_or_ready_with_probe(
    stop_seen: &std::sync::atomic::AtomicBool,
    hook_rx: &std::sync::Mutex<
        tokio::sync::broadcast::Receiver<crate::services::claude_tui::hook_server::HookEvent>,
    >,
    expected_session_id: &str,
    hook_events_after: chrono::DateTime<chrono::Utc>,
    mut ready_for_input: impl FnMut() -> bool,
) -> bool {
    if stop_seen.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    let Ok(mut rx) = hook_rx.lock() else {
        return ready_for_input();
    };
    loop {
        match rx.try_recv() {
            Ok(event)
                if event.provider == "claude"
                    && event.session_id == expected_session_id
                    && event.received_at >= hook_events_after
                    && event.kind
                        == crate::services::claude_tui::hook_server::HookEventKind::Stop =>
            {
                stop_seen.store(true, std::sync::atomic::Ordering::Relaxed);
                return true;
            }
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
    drop(rx);
    ready_for_input()
}

#[cfg(unix)]
fn claude_tui_tmux_capture_indicates_ready_for_input(tmux_session_name: &str) -> bool {
    crate::services::platform::tmux::capture_pane(tmux_session_name, -80)
        .map(|capture| crate::services::provider::tmux_capture_indicates_ready_for_input(&capture))
        .unwrap_or(false)
}

#[cfg(all(test, unix))]
mod claude_tui_ready_probe_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn ready_probe_uses_tmux_fallback_when_stop_hook_is_missing() {
        let (_tx, rx) = tokio::sync::broadcast::channel(4);
        let hook_rx = Mutex::new(rx);
        let stop_seen = AtomicBool::new(false);

        assert!(claude_tui_stop_hook_seen_or_ready_with_probe(
            &stop_seen,
            &hook_rx,
            "session-1",
            chrono::Utc::now(),
            || true
        ));
        assert!(!stop_seen.load(Ordering::Relaxed));
    }

    #[test]
    fn ready_probe_still_completes_on_matching_stop_hook() {
        let (tx, rx) = tokio::sync::broadcast::channel(4);
        let hook_rx = Mutex::new(rx);
        let stop_seen = AtomicBool::new(false);
        let hook_events_after = chrono::Utc::now() - chrono::Duration::milliseconds(1);
        tx.send(crate::services::claude_tui::hook_server::HookEvent {
            provider: "claude".to_string(),
            session_id: "session-1".to_string(),
            kind: crate::services::claude_tui::hook_server::HookEventKind::Stop,
            received_at: chrono::Utc::now(),
            payload: serde_json::json!({}),
        })
        .unwrap();

        assert!(claude_tui_stop_hook_seen_or_ready_with_probe(
            &stop_seen,
            &hook_rx,
            "session-1",
            hook_events_after,
            || false
        ));
        assert!(stop_seen.load(Ordering::Relaxed));
    }

    #[test]
    fn ready_probe_ignores_stop_hook_buffered_before_prompt_submit() {
        let (tx, rx) = tokio::sync::broadcast::channel(4);
        let hook_rx = Mutex::new(rx);
        let stop_seen = AtomicBool::new(false);
        let hook_events_after = chrono::Utc::now();
        tx.send(crate::services::claude_tui::hook_server::HookEvent {
            provider: "claude".to_string(),
            session_id: "session-1".to_string(),
            kind: crate::services::claude_tui::hook_server::HookEventKind::Stop,
            received_at: hook_events_after - chrono::Duration::milliseconds(1),
            payload: serde_json::json!({}),
        })
        .unwrap();

        assert!(!claude_tui_stop_hook_seen_or_ready_with_probe(
            &stop_seen,
            &hook_rx,
            "session-1",
            hook_events_after,
            || false
        ));
        assert!(!stop_seen.load(Ordering::Relaxed));
    }
}

/// Execute Claude inside a local tmux session with bidirectional input.
///
/// If a tmux session with this name already exists, sends the prompt as a
/// follow-up message to the running Claude process. Otherwise creates a new session.
///
/// Communication:
/// - Output: wrapper appends JSON lines to a file; parent reads with polling
/// - Input (Discord→Claude): parent writes stream-json to INPUT_FIFO
/// - Input (terminal→Claude): wrapper reads stdin directly
#[cfg(unix)]
fn execute_streaming_local_tmux(
    args: &[String],
    prompt: &str,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    compact_percent: Option<u64>,
    cache_ttl_minutes: Option<u32>,
) -> Result<(), String> {
    debug_log(&format!(
        "=== execute_streaming_local_tmux START: {} ===",
        tmux_session_name
    ));

    let output_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let input_fifo_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "input");
    let prompt_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "prompt");
    let owner_path = tmux_owner_path(tmux_session_name);

    // Check if tmux session already exists (follow-up to running session).
    // `resolve_session_temp_path` accepts either the new persistent location
    // (under `runtime_root()/runtime/sessions/`) or the legacy `/tmp/` path
    // that older wrappers still hold open fds to — so a dcserver restart
    // that lost its /tmp files does not invalidate a still-alive tmux pane.
    let session_exists = tmux_session_exists(tmux_session_name);
    let resolved_output =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "jsonl");
    let resolved_input =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "input");
    let startup_plan = classify_local_tmux_startup_plan(
        session_exists,
        tmux_session_has_live_pane(tmux_session_name),
        resolved_output.is_some(),
        resolved_input.is_some(),
    );

    if startup_plan == LocalTmuxStartupPlan::WarmFollowup {
        // Use the resolved paths (which may be the legacy /tmp path) for the
        // follow-up so we read the jsonl the live wrapper actually writes.
        let output_path = resolved_output
            .clone()
            .unwrap_or_else(|| output_path.clone());
        let input_fifo_path = resolved_input
            .clone()
            .unwrap_or_else(|| input_fifo_path.clone());
        debug_log("Existing tmux session found — sending follow-up message");
        let followup = send_followup_to_tmux(
            prompt,
            &output_path,
            &input_fifo_path,
            sender.clone(),
            cancel_token.clone(),
            tmux_session_name,
        );
        let followup = match followup {
            Ok(value) => value,
            Err(error) => {
                log_producer_exit(
                    "warm_followup_error",
                    None,
                    report_channel_id,
                    0,
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                        "error_truncated": error.chars().take(200).collect::<String>(),
                    }),
                );
                return Err(error);
            }
        };
        match followup {
            ClaudeFollowupResult::Delivered => {
                log_producer_exit(
                    "warm_followup_delivered",
                    None,
                    report_channel_id,
                    0,
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                    }),
                );
                return Ok(());
            }
            ClaudeFollowupResult::RecreateSession { error } => {
                log_producer_exit(
                    "warm_followup_recreate",
                    None,
                    report_channel_id,
                    0,
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                        "error_truncated": error.chars().take(200).collect::<String>(),
                    }),
                );
                debug_log(&format!("Follow-up failed, recreating session: {}", error));
                crate::services::termination_audit::record_termination_for_tmux(
                    tmux_session_name,
                    None,
                    "claude_provider",
                    "followup_failed_recreate",
                    Some(&format!("followup failed, recreating: {}", error)),
                    None,
                );
                record_tmux_exit_reason(
                    tmux_session_name,
                    &format!("followup failed, recreating: {}", error),
                );
                crate::services::platform::tmux::kill_session_with_reason(
                    tmux_session_name,
                    &format!("followup failed, recreating: {}", error),
                );
                // Fall through to new session creation below
            }
            ClaudeFollowupResult::FinalizeWithNotice { error, notice } => {
                debug_log(&format!(
                    "Follow-up streamed partial output before session death — suppressing replay: {}",
                    error
                ));
                crate::services::termination_audit::record_termination_for_tmux(
                    tmux_session_name,
                    None,
                    "claude_provider",
                    "followup_partial_output_no_replay",
                    Some(&format!(
                        "partial follow-up output already delivered: {}",
                        error
                    )),
                    None,
                );
                record_tmux_exit_reason(
                    tmux_session_name,
                    &format!("partial follow-up output already delivered: {}", error),
                );
                crate::services::platform::tmux::kill_session_with_reason(
                    tmux_session_name,
                    &format!("partial follow-up output already delivered: {}", error),
                );
                emit_followup_restart_suppressed_notice(&sender, &notice);
                log_producer_exit(
                    "warm_followup_finalize_notice",
                    None,
                    report_channel_id,
                    0,
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                        "error_truncated": error.chars().take(200).collect::<String>(),
                    }),
                );
                return Ok(());
            }
        }
    } else if startup_plan == LocalTmuxStartupPlan::RecreateStaleSession {
        debug_log("Stale tmux session found — recreating");
        crate::services::termination_audit::record_termination_for_tmux(
            tmux_session_name,
            None,
            "claude_provider",
            "stale_session_recreate",
            Some("stale local session cleanup before recreate"),
            None,
        );
        record_tmux_exit_reason(
            tmux_session_name,
            "stale local session cleanup before recreate",
        );
        crate::services::platform::tmux::kill_session_with_reason(
            tmux_session_name,
            "stale local session cleanup before recreate",
        );
    }

    // === Create new tmux session ===
    debug_log("No existing tmux session — creating new one");

    // Clean up any leftover files in both persistent and legacy locations.
    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);

    // Create output file (empty)
    std::fs::write(&output_path, "").map_err(|e| format!("Failed to create output file: {}", e))?;

    // Create input FIFO
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

    // Write prompt to temp file
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;
    write_tmux_owner_marker(tmux_session_name)?;

    // Get paths
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
    let resolution = resolve_claude_binary();
    let claude_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Claude CLI not found".to_string())?;

    // Build wrapper command via script file to avoid tmux "command too long" errors.
    // The system prompt in --append-system-prompt can be thousands of chars, exceeding
    // tmux's command buffer limit when passed as a direct argument.
    let escaped_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");

    let env_lines = build_tmux_launch_env_lines(
        resolution.exec_path.as_deref(),
        report_channel_id,
        report_provider,
        compact_percent,
        cache_ttl_minutes,
    );

    let script_content = format!(
        "#!/bin/bash\n\
        {env}\
        exec {exe} tmux-wrapper \\\n  \
        --output-file {output} \\\n  \
        --input-fifo {input_fifo} \\\n  \
        --prompt-file {prompt} \\\n  \
        --cwd {wd} \\\n  \
        -- {claude_bin} {claude_args}\n",
        env = env_lines,
        exe = shell_escape(&exe.display().to_string()),
        output = shell_escape(&output_path),
        input_fifo = shell_escape(&input_fifo_path),
        prompt = shell_escape(&prompt_path),
        wd = shell_escape(working_dir),
        claude_bin = shell_escape(&claude_bin),
        claude_args = escaped_args.join(" "),
    );

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write launch script: {}", e))?;

    debug_log(&format!(
        "Launch script written to {} ({} bytes)",
        script_path,
        script_content.len()
    ));

    // Launch tmux session with script file (avoids command length limits)
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

    // Keep tmux session alive after process exits for post-mortem analysis
    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    // Stamp generation marker so post-restart watcher restore can detect old sessions
    let gen_marker_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
    let current_gen = crate::services::discord::runtime_store::load_generation();
    let _ = std::fs::write(&gen_marker_path, current_gen.to_string());

    debug_log("tmux session created, storing in cancel token...");

    // Store tmux session name in cancel token
    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(tmux_session_name.to_string());
    }

    emit_fresh_session_watcher_handoff(&sender, output_path, input_fifo_path, tmux_session_name);
    log_producer_exit(
        "fresh_session_watcher_owned_handoff",
        None,
        report_channel_id,
        0,
        serde_json::json!({
            "tmux_session_name": tmux_session_name,
        }),
    );
    Ok(())
}

/// Send a follow-up message to an existing tmux Claude session.
///
/// Returns `RecreateSession` only when the follow-up failed before any new
/// output was delivered. If partial output already streamed and the session
/// then dies, the caller is asked to finalize the turn with an explicit notice
/// instead of replaying the prompt from scratch.
#[cfg(unix)]
fn send_followup_to_tmux(
    prompt: &str,
    output_path: &str,
    input_fifo_path: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<ClaudeFollowupResult, String> {
    use std::io::Write;

    debug_log(&format!(
        "=== send_followup_to_tmux: {} ===",
        tmux_session_name
    ));

    // Get current output file size (we'll read from this offset)
    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    debug_log(&format!("Output file offset: {}", start_offset));

    // Format prompt as stream-json
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": prompt
        }
    });

    // Write to input FIFO — if the pipe is broken or missing, request recreation
    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .open(input_fifo_path)
        .map_err(|e| format!("Failed to open input FIFO: {}", e))
        .and_then(|mut fifo| {
            writeln!(fifo, "{}", msg)
                .map_err(|e| format!("Failed to write to input FIFO: {}", e))?;
            fifo.flush()
                .map_err(|e| format!("Failed to flush input FIFO: {}", e))?;
            Ok(())
        });

    if let Err(e) = write_result {
        if should_recreate_session_after_followup_fifo_error(&e) {
            debug_log(&format!("FIFO error triggers session recreation: {}", e));
            return Ok(ClaudeFollowupResult::RecreateSession { error: e });
        }
        return Err(e);
    }

    debug_log("Follow-up message sent to input FIFO");

    // Store tmux session name in cancel token
    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(tmux_session_name.to_string());
    }

    // Read output file from the offset
    let read_result = read_output_file_until_result(
        output_path,
        start_offset,
        sender.clone(),
        cancel_token,
        SessionProbe::tmux(tmux_session_name.to_string()),
    )?;

    let outcome = classify_followup_result(
        read_result,
        start_offset,
        "session died during follow-up output reading",
    );
    if matches!(outcome, ClaudeFollowupResult::Delivered) {
        let current_offset = std::fs::metadata(output_path)
            .map(|meta| meta.len())
            .unwrap_or(start_offset);
        let _ = sender.send(StreamMessage::TmuxReady {
            output_path: output_path.to_string(),
            input_fifo_path: input_fifo_path.to_string(),
            tmux_session_name: tmux_session_name.to_string(),
            last_offset: current_offset,
        });
    } else if matches!(outcome, ClaudeFollowupResult::RecreateSession { .. }) {
        debug_log("tmux session died during follow-up before new output — requesting recreation");
    } else {
        debug_log("tmux session died after streaming partial follow-up output — suppress replay");
    }
    Ok(outcome)
}

/// Poll-read the output file from a given offset until a "result" event is received.
/// Uses raw File::read to handle growing file (not BufReader which caches EOF).
// ─── ProcessBackend execution path ────────────────────────────────────────────

/// Execute Claude via ProcessBackend (direct child process, no tmux).
/// Used when tmux is not available or on Windows.
pub(crate) fn execute_streaming_local_process(
    args: &[String],
    prompt: &str,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    session_name: &str,
    compact_percent: Option<u64>,
) -> Result<(), String> {
    use crate::services::session_backend::{ProcessBackend, SessionBackend, SessionConfig};

    debug_log(&format!(
        "=== execute_streaming_local_process START: {} ===",
        session_name
    ));

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

    // Check for existing process session (follow-up)
    // ProcessBackend sessions don't persist across restarts, so we track via static map
    if process_session_is_alive(session_name) {
        debug_log("Existing process session found — sending follow-up");
        match send_followup_to_process(
            prompt,
            &output_path,
            session_name,
            sender.clone(),
            cancel_token.clone(),
        )? {
            ClaudeFollowupResult::Delivered => return Ok(()),
            ClaudeFollowupResult::RecreateSession { error } => {
                debug_log(&format!(
                    "Process follow-up failed, recreating session: {}",
                    error
                ));
                if let Some(handle) = remove_process_session(session_name) {
                    terminate_process_handle(handle);
                }
            }
            ClaudeFollowupResult::FinalizeWithNotice { error, notice } => {
                debug_log(&format!(
                    "Process follow-up streamed partial output before session death — suppressing replay: {}",
                    error
                ));
                if let Some(handle) = remove_process_session(session_name) {
                    terminate_process_handle(handle);
                }
                emit_followup_restart_suppressed_notice(&sender, &notice);
                return Ok(());
            }
        }
    }

    // Clean up stale files
    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&prompt_path);

    // Write prompt
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;

    // Build wrapper args — no shell_escape here because ProcessBackend uses
    // Command::new().args() (direct argv), not a shell script.
    let resolution = resolve_claude_binary();
    let claude_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Claude CLI not found".to_string())?;
    let mut wrapper_args: Vec<String> = vec!["--".to_string(), claude_bin.to_string()];
    wrapper_args.extend(args.iter().map(|a| a.to_string()));

    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;

    let mut env_vars = resolution
        .exec_path
        .clone()
        .map(|path| vec![("PATH".to_string(), path)])
        .unwrap_or_default();
    if let Some(pct) = compact_percent.filter(|&p| p > 0) {
        env_vars.push((
            "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".to_string(),
            pct.to_string(),
        ));
    }
    let config = SessionConfig {
        session_name: session_name.to_string(),
        working_dir: working_dir.to_string(),
        agentdesk_exe: exe.display().to_string(),
        output_path: output_path.clone(),
        prompt_path: prompt_path.clone(),
        wrapper_subcommand: "tmux-wrapper".to_string(),
        wrapper_args,
        env_vars,
    };

    let backend = ProcessBackend::new();
    let handle = backend.create_session(&config)?;

    // Store child PID in cancel token
    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle.pid());
    }

    // Store handle for follow-up messages
    insert_process_session(session_name.to_string(), handle);

    // Poll output file until result
    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token,
        process_session_probe(session_name),
    )?;

    fold_read_output_result(
        read_result,
        |offset| {
            let _ = sender.send(StreamMessage::ProcessReady {
                output_path,
                session_name: session_name.to_string(),
                last_offset: offset,
            });
        },
        |_| {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 프로세스가 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
            remove_process_session(session_name);
        },
    );

    debug_log("=== execute_streaming_local_process END ===");
    Ok(())
}

/// Send a follow-up message to an existing ProcessBackend session.
fn send_followup_to_process(
    prompt: &str,
    output_path: &str,
    session_name: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<ClaudeFollowupResult, String> {
    use crate::services::tmux_diagnostics::should_recreate_session_after_stdin_error;

    debug_log(&format!(
        "=== send_followup_to_process: {} ===",
        session_name
    ));

    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    // Format and send via stdin pipe
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": prompt
        }
    });

    if let Err(e) = send_process_session_input(session_name, &msg.to_string()) {
        if should_recreate_session_after_stdin_error(&e) {
            debug_log(&format!(
                "stdin pipe error triggers session recreation: {}",
                e
            ));
            return Ok(ClaudeFollowupResult::RecreateSession { error: e });
        }
        return Err(e);
    }

    // Store session in cancel token
    if let Some(ref token) = cancel_token {
        if let Some(pid) = process_session_pid(session_name) {
            *token.child_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
        }
    }

    let read_result = read_output_file_until_result(
        output_path,
        start_offset,
        sender.clone(),
        cancel_token,
        process_session_probe(session_name),
    )?;

    let outcome = classify_followup_result(
        read_result,
        start_offset,
        "process died during follow-up output reading",
    );
    if matches!(outcome, ClaudeFollowupResult::Delivered) {
        let current_offset = std::fs::metadata(output_path)
            .map(|meta| meta.len())
            .unwrap_or(start_offset);
        let _ = sender.send(StreamMessage::ProcessReady {
            output_path: output_path.to_string(),
            session_name: session_name.to_string(),
            last_offset: current_offset,
        });
    } else if matches!(outcome, ClaudeFollowupResult::RecreateSession { .. }) {
        debug_log(
            "process session died during follow-up before new output — requesting recreation",
        );
        remove_process_session(session_name);
    } else {
        debug_log(
            "process session died after streaming partial follow-up output — suppress replay",
        );
        remove_process_session(session_name);
    }
    Ok(outcome)
}

pub fn terminate_local_session(session_name: &str) {
    if let Some(pid) = process_session_pid(session_name) {
        remove_process_session(session_name);
        kill_pid_tree(pid);
    }

    #[cfg(unix)]
    if tmux_session_exists(session_name) {
        record_tmux_exit_reason(session_name, "model change requested fresh session");
        let _ = crate::services::platform::tmux::kill_session_with_reason(
            session_name,
            "model change requested fresh session",
        );
    }
}

/// Execute Claude inside a tmux session on a remote host via SSH.
/// NOTE: Remote SSH execution is not available in AgentDesk — always returns Err.
#[cfg(unix)]
fn execute_streaming_remote_tmux(
    _profile: &RemoteProfile,
    _args: &[String],
    _prompt: &str,
    _working_dir: &str,
    _sender: Sender<StreamMessage>,
    _cancel_token: Option<std::sync::Arc<CancelToken>>,
    _tmux_session_name: &str,
) -> Result<(), String> {
    Err("Remote SSH tmux execution is not available in AgentDesk".to_string())
}

#[cfg(all(test, unix))]
mod local_tmux_lifecycle_tests {
    use super::*;

    #[test]
    fn local_tmux_plan_uses_warm_followup_only_with_live_pane_and_runtime_paths() {
        assert_eq!(
            classify_local_tmux_startup_plan(true, true, true, true),
            LocalTmuxStartupPlan::WarmFollowup,
            "warm follow-up is the only path where an existing wrapper is usable"
        );

        for (has_live_pane, has_output_path, has_input_fifo_path) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            assert_eq!(
                classify_local_tmux_startup_plan(
                    true,
                    has_live_pane,
                    has_output_path,
                    has_input_fifo_path,
                ),
                LocalTmuxStartupPlan::RecreateStaleSession,
                "existing tmux sessions missing live ownership evidence must be killed and recreated"
            );
        }
    }

    #[test]
    fn local_tmux_plan_keeps_cold_start_on_watcher_handoff_path() {
        assert_eq!(
            classify_local_tmux_startup_plan(false, false, false, false),
            LocalTmuxStartupPlan::ColdStart
        );
        assert_eq!(
            classify_local_tmux_startup_plan(false, true, true, true),
            LocalTmuxStartupPlan::ColdStart,
            "impossible live-pane evidence without session_exists stays on the safe cold path"
        );
    }

    #[test]
    fn fresh_session_watcher_handoff_starts_at_jsonl_offset_zero() {
        let (sender, receiver) = std::sync::mpsc::channel();
        emit_fresh_session_watcher_handoff(
            &sender,
            "/tmp/session.jsonl".to_string(),
            "/tmp/session.input".to_string(),
            "claude-test",
        );

        let message = receiver.recv().unwrap();
        match message {
            StreamMessage::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name,
                last_offset,
            } => {
                assert_eq!(output_path, "/tmp/session.jsonl");
                assert_eq!(input_fifo_path, "/tmp/session.input");
                assert_eq!(tmux_session_name, "claude-test");
                assert_eq!(
                    last_offset, 0,
                    "fresh cold-start watcher must consume JSONL from the beginning"
                );
            }
            other => panic!("expected TmuxReady, got {other:?}"),
        }
    }

    #[test]
    fn fresh_tui_start_offset_skips_existing_transcript_for_fresh_launch() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let transcript_path = temp_dir.path().join("session.jsonl");
        let stale_transcript = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"stale-hidden"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","result":"stale done","session_id":"stale-session"}"#,
            "\n"
        );
        std::fs::write(&transcript_path, stale_transcript).unwrap();

        let start_offset = claude_tui_fresh_turn_start_offset(&transcript_path);
        assert_eq!(start_offset, stale_transcript.len() as u64);

        let mut transcript = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript_path)
            .unwrap();
        writeln!(
            transcript,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"fresh-visible"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            transcript,
            r#"{{"type":"result","subtype":"success","result":"fresh done","session_id":"fresh-session"}}"#
        )
        .unwrap();
        drop(transcript);

        let (sender, receiver) = std::sync::mpsc::channel();
        let result = read_output_file_until_result(
            transcript_path.to_str().unwrap(),
            start_offset,
            sender,
            None,
            SessionProbe::new(|| true, || false),
        )
        .unwrap();

        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        let messages: Vec<_> = receiver.try_iter().collect();
        assert!(
            !messages.iter().any(
                |message| matches!(message, StreamMessage::Text { content } if content.contains("stale-hidden"))
            ),
            "stale transcript content must not be replayed: {messages:?}"
        );
        assert!(
            messages.iter().any(
                |message| matches!(message, StreamMessage::Text { content } if content == "fresh-visible")
            ),
            "new turn text should still be delivered: {messages:?}"
        );
        assert!(
            messages.iter().any(
                |message| matches!(message, StreamMessage::Done { result, session_id } if result == "fresh done" && session_id.as_deref() == Some("fresh-session"))
            ),
            "new turn result should complete the read: {messages:?}"
        );
    }

    #[test]
    fn fresh_tui_prompt_retry_is_limited_to_readiness_timeouts() {
        assert!(should_retry_claude_tui_fresh_prompt_ready(
            "timeout waiting for claude tui fresh prompt input readiness after 120s",
            1
        ));
        assert!(!should_retry_claude_tui_fresh_prompt_ready(
            "timeout waiting for claude tui fresh prompt input readiness after 120s",
            CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS
        ));
        assert!(!should_retry_claude_tui_fresh_prompt_ready(
            "claude tui session died before prompt input was ready",
            1
        ));
    }

    #[test]
    fn fresh_tui_prompt_retry_backoff_scales_by_completed_attempts() {
        assert_eq!(
            claude_tui_fresh_prompt_ready_backoff(1),
            Duration::from_secs(5)
        );
        assert_eq!(
            claude_tui_fresh_prompt_ready_backoff(2),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn claude_tui_turn_lock_serializes_same_tmux_session() {
        let session_name = format!("claude-tui-lock-{}", uuid::Uuid::new_v4());
        let first_lock = claude_tui_session_turn_lock(&session_name);
        let second_lock = claude_tui_session_turn_lock(&session_name);
        assert!(
            Arc::ptr_eq(&first_lock, &second_lock),
            "same tmux session must share one turn lock"
        );

        let _guard = first_lock.lock().unwrap_or_else(|error| error.into_inner());
        let (sender, receiver) = std::sync::mpsc::channel();
        let session_name_for_thread = session_name.clone();
        let handle = std::thread::spawn(move || {
            let lock = claude_tui_session_turn_lock(&session_name_for_thread);
            let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
            sender.send(()).unwrap();
        });

        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "concurrent same-session turn entered before the first guard dropped"
        );
        drop(_guard);
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("same-session turn should enter after the first guard drops");
        handle.join().unwrap();
    }

    #[test]
    fn claude_tui_turn_lock_is_per_tmux_session() {
        let first =
            claude_tui_session_turn_lock(&format!("claude-tui-lock-a-{}", uuid::Uuid::new_v4()));
        let second =
            claude_tui_session_turn_lock(&format!("claude-tui-lock-b-{}", uuid::Uuid::new_v4()));

        assert!(
            !Arc::ptr_eq(&first, &second),
            "different tmux sessions must not share one turn lock"
        );
    }
}

#[cfg(all(test, unix))]
mod claude_tui_session_resolution_tests {
    use super::*;

    #[test]
    fn preserves_existing_resume_transcript() {
        let cwd = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &session_id,
            Some(claude_home.path()),
        )
        .unwrap();
        std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        std::fs::write(&transcript_path, "").unwrap();

        let resolution = resolve_claude_tui_session_for_launch(
            cwd.path(),
            Some(&session_id),
            Some(claude_home.path()),
        )
        .unwrap();

        assert!(resolution.resume);
        assert_eq!(resolution.session_id, session_id);
        assert_eq!(resolution.transcript_path, transcript_path);
    }

    #[test]
    fn forces_fresh_when_resume_transcript_missing() {
        let cwd = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let stale_session_id = uuid::Uuid::new_v4().to_string();

        let resolution = resolve_claude_tui_session_for_launch(
            cwd.path(),
            Some(&stale_session_id),
            Some(claude_home.path()),
        )
        .unwrap();

        assert!(!resolution.resume);
        assert_ne!(resolution.session_id, stale_session_id);
        assert!(uuid::Uuid::parse_str(&resolution.session_id).is_ok());
        let expected_filename = format!("{}.jsonl", resolution.session_id);
        assert_eq!(
            resolution
                .transcript_path
                .file_name()
                .and_then(|name| name.to_str()),
            Some(expected_filename.as_str())
        );
    }

    #[test]
    fn forced_fresh_resolution_still_skips_existing_transcript_bytes() {
        let cwd = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let missing_resume_session_id = uuid::Uuid::new_v4().to_string();

        let resolution = resolve_claude_tui_session_for_launch(
            cwd.path(),
            Some(&missing_resume_session_id),
            Some(claude_home.path()),
        )
        .unwrap();
        assert!(!resolution.resume);

        let stale_transcript = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"forced-fresh-stale"}]}}"#,
            "\n"
        );
        std::fs::create_dir_all(resolution.transcript_path.parent().unwrap()).unwrap();
        std::fs::write(&resolution.transcript_path, stale_transcript).unwrap();

        assert_eq!(
            claude_tui_fresh_turn_start_offset(&resolution.transcript_path),
            stale_transcript.len() as u64
        );
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::services::discord::restart_report::{
        RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
    };

    // ========== is_valid_session_id tests ==========

    #[test]
    fn test_tmux_launch_env_lines_include_exec_path_and_report_envs() {
        let env_lines = build_tmux_launch_env_lines(
            Some("/tmp/provider:/usr/bin"),
            Some(7),
            Some(ProviderKind::Claude),
            None,
            None,
        );

        assert!(env_lines.contains("unset CLAUDECODE"));
        assert!(env_lines.contains("export PATH='/tmp/provider:/usr/bin'"));
        assert!(env_lines.contains(&format!("export {}=7", RESTART_REPORT_CHANNEL_ENV)));
        assert!(env_lines.contains(&format!("export {}=claude", RESTART_REPORT_PROVIDER_ENV)));
        // Default 5m TTL → no env var emitted.
        assert!(!env_lines.contains("CLAUDE_CODE_EXTENDED_CACHE_TTL"));
    }

    #[test]
    fn test_tmux_launch_env_lines_emit_extended_cache_ttl_for_60min_bucket() {
        let env_lines = build_tmux_launch_env_lines(None, None, None, None, Some(60));
        assert!(
            env_lines.contains("export CLAUDE_CODE_EXTENDED_CACHE_TTL=1h"),
            "expected 1h TTL env var, got: {env_lines}"
        );
    }

    #[test]
    fn test_tmux_launch_env_lines_default_ttl_emits_no_extended_cache_env() {
        let env_lines = build_tmux_launch_env_lines(None, None, None, None, Some(5));
        assert!(
            !env_lines.contains("CLAUDE_CODE_EXTENDED_CACHE_TTL"),
            "5m default TTL must NOT emit env var, got: {env_lines}"
        );
    }

    #[test]
    fn test_tmux_launch_env_lines_invalid_ttl_falls_back_to_default() {
        // Anything other than 5/60 must be rejected by normalize_cache_ttl_minutes
        // and treated as the default 5m bucket → no env var emitted.
        for invalid in [0u32, 1, 4, 6, 30, 59, 61, 120] {
            let env_lines = build_tmux_launch_env_lines(None, None, None, None, Some(invalid));
            assert!(
                !env_lines.contains("CLAUDE_CODE_EXTENDED_CACHE_TTL"),
                "invalid TTL {invalid} leaked env var: {env_lines}"
            );
        }
    }

    #[test]
    fn test_cache_ttl_env_value_normalizes_buckets() {
        assert_eq!(cache_ttl_env_value(None), None);
        assert_eq!(cache_ttl_env_value(Some(5)), None);
        assert_eq!(cache_ttl_env_value(Some(60)), Some("1h"));
        assert_eq!(cache_ttl_env_value(Some(0)), None);
        assert_eq!(cache_ttl_env_value(Some(120)), None);
    }

    #[test]
    fn test_append_claude_mcp_config_arg_skips_when_no_runtime_config() {
        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let previous_config = std::env::var_os("AGENTDESK_CONFIG");
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_memento_access_key = std::env::var_os("MEMENTO_ACCESS_KEY");
        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        unsafe { std::env::remove_var("MEMENTO_ACCESS_KEY") };
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("agentdesk.yaml");
        crate::config::save_to_path(&config_path, &crate::config::Config::default()).unwrap();
        unsafe { std::env::set_var("AGENTDESK_CONFIG", &config_path) };

        let mut args = vec!["-p".to_string()];
        append_claude_mcp_config_arg(&mut args, None);

        assert_eq!(args, vec!["-p".to_string()]);

        match previous_config {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_CONFIG", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_CONFIG") },
        }
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_memento_access_key {
            Some(value) => unsafe { std::env::set_var("MEMENTO_ACCESS_KEY", value) },
            None => unsafe { std::env::remove_var("MEMENTO_ACCESS_KEY") },
        }
    }

    #[test]
    fn test_append_claude_fast_mode_arg_sets_explicit_state() {
        let mut args = Vec::new();
        append_claude_fast_mode_arg(&mut args, Some(true));
        assert_eq!(
            args,
            vec!["--settings".to_string(), r#"{"fastMode":true}"#.to_string(),]
        );

        let mut disabled_args = Vec::new();
        append_claude_fast_mode_arg(&mut disabled_args, Some(false));
        assert_eq!(
            disabled_args,
            vec![
                "--settings".to_string(),
                r#"{"fastMode":false}"#.to_string(),
            ]
        );
    }

    #[test]
    fn test_append_claude_fast_mode_arg_skips_when_unset() {
        let mut args = Vec::new();
        append_claude_fast_mode_arg(&mut args, None);
        assert!(args.is_empty());
    }

    #[test]
    fn test_session_id_valid() {
        assert!(is_valid_session_id("abc123"));
        assert!(is_valid_session_id("session-1"));
        assert!(is_valid_session_id("session_2"));
        assert!(is_valid_session_id("ABC-XYZ_123"));
        assert!(is_valid_session_id("a")); // Single char
    }

    #[test]
    fn test_session_id_empty_rejected() {
        assert!(!is_valid_session_id(""));
    }

    #[test]
    fn test_session_id_too_long_rejected() {
        // 64 characters should be valid
        let max_len = "a".repeat(64);
        assert!(is_valid_session_id(&max_len));

        // 65 characters should be rejected
        let too_long = "a".repeat(65);
        assert!(!is_valid_session_id(&too_long));
    }

    #[test]
    fn test_session_id_special_chars_rejected() {
        assert!(!is_valid_session_id("session;rm -rf"));
        assert!(!is_valid_session_id("session'OR'1=1"));
        assert!(!is_valid_session_id("session`cmd`"));
        assert!(!is_valid_session_id("session$(cmd)"));
        assert!(!is_valid_session_id("session\nline2"));
        assert!(!is_valid_session_id("session\0null"));
        assert!(!is_valid_session_id("path/traversal"));
        assert!(!is_valid_session_id("session with space"));
        assert!(!is_valid_session_id("session.dot"));
        assert!(!is_valid_session_id("session@email"));
    }

    #[test]
    fn test_session_id_unicode_rejected() {
        assert!(!is_valid_session_id("세션아이디"));
        assert!(!is_valid_session_id("session_日本語"));
        assert!(!is_valid_session_id("émoji🎉"));
    }
    // ========== ClaudeResponse tests ==========

    #[test]
    fn test_claude_response_struct() {
        let response = ClaudeResponse {
            success: true,
            response: Some("Hello".to_string()),
            session_id: Some("abc123".to_string()),
            error: None,
        };

        assert!(response.success);
        assert_eq!(response.response, Some("Hello".to_string()));
        assert_eq!(response.session_id, Some("abc123".to_string()));
        assert!(response.error.is_none());
    }

    #[test]
    fn test_claude_response_error() {
        let response = ClaudeResponse {
            success: false,
            response: None,
            session_id: None,
            error: Some("Connection failed".to_string()),
        };

        assert!(!response.success);
        assert!(response.response.is_none());
        assert_eq!(response.error, Some("Connection failed".to_string()));
    }

    // ========== parse_claude_output tests ==========

    #[test]
    fn test_parse_claude_output_json_result() {
        let output = r#"{"session_id": "test-123", "result": "Hello, world!"}"#;
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(response.response, Some("Hello, world!".to_string()));
        assert_eq!(response.session_id, Some("test-123".to_string()));
    }

    #[test]
    fn test_parse_claude_output_json_message() {
        let output = r#"{"session_id": "sess-456", "message": "This is a message"}"#;
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(response.response, Some("This is a message".to_string()));
    }

    #[test]
    fn test_parse_claude_output_plain_text() {
        let output = "Just plain text response";
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(
            response.response,
            Some("Just plain text response".to_string())
        );
    }

    #[test]
    fn test_parse_claude_output_multiline() {
        let output = r#"{"session_id": "s1"}
{"result": "Final result"}"#;
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(response.session_id, Some("s1".to_string()));
        assert_eq!(response.response, Some("Final result".to_string()));
    }

    #[test]
    fn test_parse_claude_output_empty() {
        let output = "";
        let response = parse_claude_output(output);

        assert!(response.success);
        // Empty output should return empty response
        assert_eq!(response.response, Some("".to_string()));
    }

    // ========== is_ai_supported tests ==========

    #[test]
    fn test_is_ai_supported() {
        #[cfg(any(unix, windows))]
        assert!(is_ai_supported());

        #[cfg(not(any(unix, windows)))]
        assert!(!is_ai_supported());
    }

    // ========== parse_stream_message tests ==========

    #[test]
    fn test_parse_stream_message_init() {
        let json: Value =
            serde_json::from_str(r#"{"type":"system","subtype":"init","session_id":"test-123"}"#)
                .unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::Init { session_id, .. }) => {
                assert_eq!(session_id, "test-123");
            }
            _ => panic!("Expected Init message"),
        }
    }

    #[test]
    fn test_parse_stream_message_text() {
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}]}}"#,
        )
        .unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::Text { content }) => {
                assert_eq!(content, "Hello world");
            }
            _ => panic!("Expected Text message"),
        }
    }

    #[test]
    fn test_parse_stream_message_tool_use() {
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#
        ).unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::ToolUse { name, input }) => {
                assert_eq!(name, "Bash");
                assert!(input.contains("ls"));
            }
            _ => panic!("Expected ToolUse message"),
        }
    }

    #[test]
    fn test_parse_stream_message_tool_result() {
        let json: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"file.txt","is_error":false}]}}"#
        ).unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::ToolResult { content, is_error }) => {
                assert_eq!(content, "file.txt");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolResult message"),
        }
    }

    #[test]
    fn test_parse_stream_message_tool_result_error() {
        let json: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"Error: not found","is_error":true}]}}"#
        ).unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::ToolResult { content, is_error }) => {
                assert_eq!(content, "Error: not found");
                assert!(is_error);
            }
            _ => panic!("Expected ToolResult message with error"),
        }
    }

    #[test]
    fn test_parse_stream_message_result() {
        let json: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","result":"Done!","session_id":"sess-456"}"#,
        )
        .unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::Done { result, session_id }) => {
                assert_eq!(result, "Done!");
                assert_eq!(session_id, Some("sess-456".to_string()));
            }
            _ => panic!("Expected Done message"),
        }
    }

    #[test]
    fn test_parse_stream_message_unknown_type() {
        let json: Value = serde_json::from_str(r#"{"type":"unknown","data":"something"}"#).unwrap();

        let msg = parse_stream_message(&json);
        assert!(msg.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn test_tmux_capture_detects_ready_prompt() {
        let capture = "...\n▶ Ready for input (type message + Enter)\n";
        assert!(crate::services::provider::tmux_capture_indicates_ready_for_input(capture));
    }

    #[test]
    #[cfg(unix)]
    fn test_tmux_capture_detects_claude_tui_ready_prompt() {
        let capture = "\
previous output\n\
─────────────────────────────────────────────────────────────────────────────\n\
❯\u{00a0}\n\
─────────────────────────────────────────────────────────────────────────────\n\
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%\n\
  📁 agentdesk (main*) │ Todos: -\n\
  ⏵⏵ bypass permissions on";
        assert!(crate::services::provider::tmux_capture_indicates_ready_for_input(capture));
    }

    #[test]
    #[cfg(unix)]
    fn test_tmux_capture_ignores_non_ready_prompt() {
        let capture = "Claude is still working...\n";
        assert!(!crate::services::provider::tmux_capture_indicates_ready_for_input(capture));
    }

    // ========== parse_stream_message thinking tests ==========

    #[test]
    fn test_parse_thinking_only() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Let me analyze this"}]}}"#
        ).unwrap();
        let msg = parse_stream_message(&json).unwrap();
        match msg {
            StreamMessage::Thinking { summary } => {
                assert!(summary.is_none());
            }
            _ => panic!("Expected Thinking"),
        }
    }

    #[test]
    fn test_parse_thinking_with_text_returns_text() {
        // When content has [thinking, text], text should be returned (not Thinking)
        let json: serde_json::Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"internal"},{"type":"text","text":"visible answer"}]}}"#
        ).unwrap();
        let msg = parse_stream_message(&json).unwrap();
        match msg {
            StreamMessage::Text { content } => assert_eq!(content, "visible answer"),
            _ => panic!("Expected Text, got thinking or other"),
        }
    }

    #[test]
    fn test_parse_thinking_with_tool_use_returns_tool() {
        // When content has [thinking, tool_use], tool_use should be returned
        let json: serde_json::Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"planning"},{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/test"}}]}}"#
        ).unwrap();
        let msg = parse_stream_message(&json).unwrap();
        match msg {
            StreamMessage::ToolUse { name, .. } => assert_eq!(name, "Read"),
            _ => panic!("Expected ToolUse"),
        }
    }

    // ========== parse_assistant_extra_tool_uses tests ==========

    #[test]
    fn test_extra_tool_uses_text_and_tool() {
        // When content has [text, tool_use], parse_stream_message returns Text;
        // parse_assistant_extra_tool_uses should return the tool_use.
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"이슈를 생성합니다."},{"type":"tool_use","name":"Bash","input":{"command":"echo hi"}}]}}"#
        ).unwrap();

        // Primary returns Text
        let primary = parse_stream_message(&json).unwrap();
        assert!(matches!(primary, StreamMessage::Text { .. }));

        // Extra returns the ToolUse
        let extras = parse_assistant_extra_tool_uses(&json);
        assert_eq!(extras.len(), 1);
        match &extras[0] {
            StreamMessage::ToolUse { name, .. } => assert_eq!(name, "Bash"),
            _ => panic!("Expected ToolUse"),
        }
    }

    #[test]
    fn test_extra_tool_uses_text_only() {
        // When content has only text, no extra tool_uses.
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#,
        )
        .unwrap();
        let extras = parse_assistant_extra_tool_uses(&json);
        assert!(extras.is_empty());
    }

    #[test]
    fn test_extra_tool_uses_tool_only() {
        // When content has only tool_use (no preceding text), no extras
        // because parse_stream_message would have returned the tool_use directly.
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/tmp"}}]}}"#
        ).unwrap();
        let extras = parse_assistant_extra_tool_uses(&json);
        assert!(extras.is_empty());
    }

    #[test]
    fn test_extra_tool_uses_text_and_multiple_tools() {
        // [text, tool_use, tool_use] — should return both tool_uses
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"작업 시작"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}},{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/a"}}]}}"#
        ).unwrap();
        let extras = parse_assistant_extra_tool_uses(&json);
        assert_eq!(extras.len(), 2);
        match &extras[0] {
            StreamMessage::ToolUse { name, .. } => assert_eq!(name, "Bash"),
            _ => panic!("Expected Bash"),
        }
        match &extras[1] {
            StreamMessage::ToolUse { name, .. } => assert_eq!(name, "Read"),
            _ => panic!("Expected Read"),
        }
    }

    #[test]
    fn test_extra_tool_uses_non_assistant() {
        // Non-assistant types should return empty.
        let json: Value =
            serde_json::from_str(r#"{"type":"result","subtype":"success","result":"ok"}"#).unwrap();
        let extras = parse_assistant_extra_tool_uses(&json);
        assert!(extras.is_empty());
    }

    // ========== Follow-up recovery tests ==========

    #[test]
    fn test_classify_followup_result_maps_completed_to_delivered() {
        let read_result = ReadOutputResult::Completed { offset: 100 };
        let followup = classify_followup_result(read_result, 100, "died");
        assert!(matches!(followup, ClaudeFollowupResult::Delivered));
    }

    #[test]
    fn test_classify_followup_result_recreates_when_no_new_output_was_streamed() {
        let read_result = ReadOutputResult::SessionDied { offset: 42 };
        let followup = classify_followup_result(
            read_result,
            42,
            "session died during follow-up output reading",
        );
        match followup {
            ClaudeFollowupResult::RecreateSession { error } => {
                assert!(error.contains("session died"));
            }
            _ => panic!("Expected RecreateSession"),
        }
    }

    #[test]
    fn test_classify_followup_result_suppresses_replay_after_partial_output() {
        let read_result = ReadOutputResult::SessionDied { offset: 84 };
        let followup = classify_followup_result(
            read_result,
            42,
            "session died during follow-up output reading",
        );
        match followup {
            ClaudeFollowupResult::FinalizeWithNotice { error, notice } => {
                assert!(error.contains("session died"));
                assert_eq!(notice, FOLLOWUP_PARTIAL_OUTPUT_NOTICE);
            }
            _ => panic!("Expected FinalizeWithNotice"),
        }
    }

    #[test]
    fn test_emit_followup_restart_suppressed_notice_sends_text_then_done() {
        use std::sync::mpsc;

        let (sender, receiver) = mpsc::channel();
        emit_followup_restart_suppressed_notice(&sender, FOLLOWUP_PARTIAL_OUTPUT_NOTICE);

        match receiver.recv().unwrap() {
            StreamMessage::Text { content } => {
                assert!(content.contains(FOLLOWUP_PARTIAL_OUTPUT_NOTICE));
            }
            other => panic!("Expected Text notice, got {:?}", other),
        }

        match receiver.recv().unwrap() {
            StreamMessage::Done { result, session_id } => {
                assert!(result.is_empty());
                assert!(session_id.is_none());
            }
            other => panic!("Expected Done after notice, got {:?}", other),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_followup_fifo_not_found_returns_recreate() {
        use std::sync::mpsc;

        let (sender, _receiver) = mpsc::channel();
        let dir = std::env::temp_dir();
        let output_path = dir.join(format!(
            "agentdesk-test-followup-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::write(&output_path, "");

        let result = send_followup_to_tmux(
            "test prompt",
            output_path.to_str().unwrap(),
            "/tmp/agentdesk-test-nonexistent-fifo-path",
            sender,
            None,
            "test-session-followup",
        );

        let _ = std::fs::remove_file(&output_path);

        match result {
            Ok(ClaudeFollowupResult::RecreateSession { error }) => {
                assert!(error.contains("Failed to open input FIFO"));
            }
            other => panic!("Expected Ok(RecreateSession), got {:?}", other),
        }
    }
}
