use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
#[cfg(unix)]
use std::time::Duration;

use crate::services::agent_protocol::{RuntimeHandoff, StreamMessage, is_valid_session_id};
use crate::services::claude_command::{ClaudeCommandBuilder, ClaudeLaunchEnv, ClaudeLaunchIntent};
use crate::services::claude_compact_context::{
    append_auto_compact_window_shell_env, apply_auto_compact_window_to_command,
    claude_model_from_args, launch_auto_compact_window_for_session,
};
#[cfg(unix)]
use crate::services::claude_tui::hosting::{
    ClaudeTuiWarmFollowupOutcome, emit_claude_tui_zero_harvest, try_claude_tui_warm_followup,
};
use crate::services::discord::restart_report::{
    RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
};
use crate::services::process::{kill_child_tree, kill_pid_tree, shell_escape};
use crate::services::provider::{
    CancelToken, ProviderKind, ReadOutputResult, SessionProbe, cancel_requested,
    cancel_token_claude_interrupt::{
        observe_claude_wrapper_followup, submit_claude_wrapper_followup,
    },
    fold_read_output_result, register_child_pid, spawn_cancel_watchdog,
};
use crate::services::provider_hosting::ProviderSessionDriver;
use crate::services::remote::RemoteProfile;
use crate::services::session_backend::{
    ReadHarvestStats, StreamLineState, emit_status_events_from_stream_json,
    insert_process_session_and_mark_active_turn, mark_process_session_active_turn,
    observe_stream_context, parse_assistant_extra_tool_uses, parse_stream_message_with_state,
    process_session_available_for_followup, process_session_pid, process_session_probe,
    read_output_file_until_result, read_output_file_until_result_with_harvest,
    remove_process_session, send_process_session_input, terminate_process_handle,
};
mod active_usage;
#[cfg(unix)]
mod backend_routing;
use self::active_usage::{AssistantUsageState, observe_assistant_usage};
#[cfg(unix)]
use self::backend_routing::{
    LocalTmuxStartupPlan, classify_local_tmux_startup_plan, cleanup_process_backend_before_tmux,
    prepare_tmux_backend_after_refused_process_demotion, process_backend_demotion_guard_liveness,
    should_preserve_live_reused_provider_session, should_refuse_process_backend_demotion,
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
const CLAUDE_TUI_TRANSCRIPT_INITIAL_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(unix)]
const CLAUDE_TUI_TRANSCRIPT_INITIAL_WAIT_MAX_INTERVAL: Duration = Duration::from_millis(500);

const CLAUDE_TUI_FOLLOWUP_REQUEUE_ENV: &str = "AGENTDESK_CLAUDE_TUI_FOLLOWUP_REQUEUE";

/// Default ON; set `AGENTDESK_CLAUDE_TUI_FOLLOWUP_REQUEUE` to `0`, `false`,
/// `off`, `no`, `disable`, or `disabled` for emergency opt-out.
pub(crate) fn claude_tui_followup_requeue_enabled() -> bool {
    let Ok(value) = std::env::var(CLAUDE_TUI_FOLLOWUP_REQUEUE_ENV) else {
        return true;
    };
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no" | "disable" | "disabled"
    )
}

#[cfg(test)]
mod claude_tui_followup_requeue_flag_tests {
    use super::*;

    struct EnvRestore {
        previous: Option<String>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous.as_deref() {
                Some(value) => unsafe {
                    std::env::set_var(CLAUDE_TUI_FOLLOWUP_REQUEUE_ENV, value);
                },
                None => unsafe {
                    std::env::remove_var(CLAUDE_TUI_FOLLOWUP_REQUEUE_ENV);
                },
            }
        }
    }

    fn with_requeue_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared test env lock poisoned");
        let _restore = EnvRestore {
            previous: std::env::var(CLAUDE_TUI_FOLLOWUP_REQUEUE_ENV).ok(),
        };
        match value {
            Some(value) => unsafe {
                std::env::set_var(CLAUDE_TUI_FOLLOWUP_REQUEUE_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(CLAUDE_TUI_FOLLOWUP_REQUEUE_ENV);
            },
        }
        f()
    }

    #[test]
    fn followup_requeue_defaults_on_when_env_is_unset() {
        with_requeue_env(None, || assert!(claude_tui_followup_requeue_enabled()));
    }

    #[test]
    fn followup_requeue_respects_emergency_opt_out_values() {
        for value in ["0", "false", "FALSE", "off", "no", "disable", "disabled"] {
            with_requeue_env(Some(value), || {
                assert!(
                    !claude_tui_followup_requeue_enabled(),
                    "{value} should disable follow-up requeue"
                );
            });
        }
    }

    #[test]
    fn followup_requeue_keeps_legacy_opt_in_and_invalid_values_enabled() {
        for value in ["1", "true", "TRUE", "on", "yes", "unexpected"] {
            with_requeue_env(Some(value), || {
                assert!(
                    claude_tui_followup_requeue_enabled(),
                    "{value} should leave follow-up requeue enabled"
                );
            });
        }
    }
}

type ClaudeResolution = (
    crate::services::claude_command::ClaudeBinary,
    crate::services::platform::BinaryResolution,
);

fn resolve_claude_binary() -> Result<ClaudeResolution, String> {
    crate::services::claude_command::ClaudeBinary::resolve()
}

pub(crate) fn append_claude_mcp_config_arg(args: &mut Vec<String>, dispatch_type: Option<&str>) {
    if let Some(config_json) = crate::services::mcp_config::claude_mcp_config_arg(dispatch_type) {
        args.push("--mcp-config".to_string());
        args.push(config_json);
    }
}

pub(crate) fn append_claude_fast_mode_arg(args: &mut Vec<String>, fast_mode_enabled: Option<bool>) {
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
    auto_compact_window: Option<u64>,
    launch_env: &ClaudeLaunchEnv,
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
    // Chokepoint base (#4559): resolved gateway launch env + managed-launch
    // marker so the `agentdesk tmux-wrapper` reconstructs this decision rather
    // than re-resolving config-less to a bare Scrub.
    launch_env.append_shell_env(&mut env_lines);
    crate::services::claude_command::append_managed_launch_marker_shell(&mut env_lines);
    // Compact-window overlay (#4591): fence off any inherited absolute window
    // and export the freshly resolved one when present.
    append_auto_compact_window_shell_env(&mut env_lines, auto_compact_window);

    env_lines
}

#[cfg(test)]
mod launch_env_tests {
    use super::build_tmux_launch_env_lines;
    use crate::services::claude_command::{ClaudeLaunchEnv, TMUX_WRAPPER_GATEWAY_RESOLVED_ENV};

    #[test]
    fn launch_env_exports_absolute_compact_window_and_gates_gateway_proxy() {
        let gateway_env = ClaudeLaunchEnv::inject_for_test("http://proxy.example/it's-ready");
        let enabled = build_tmux_launch_env_lines(None, None, None, Some(700_000), &gateway_env);
        assert!(enabled.contains("export CLAUDE_CODE_AUTO_COMPACT_WINDOW=700000\n"));
        // Managed launches always mark the wrapper env so it reconstructs this
        // decision rather than re-resolving to a bare Scrub.
        assert!(enabled.contains(&format!("export {TMUX_WRAPPER_GATEWAY_RESOLVED_ENV}=1\n")));
        assert!(
            enabled.contains("export ANTHROPIC_BASE_URL='http://proxy.example/it'\\''s-ready'\n")
        );
        assert!(enabled.contains("export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1\n"));

        let scrub = ClaudeLaunchEnv::scrub_for_test();
        let disabled = build_tmux_launch_env_lines(None, None, None, None, &scrub);
        assert!(disabled.contains("unset ANTHROPIC_BASE_URL\n"));
        assert!(disabled.contains("unset CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY\n"));

        assert!(disabled.contains("unset CLAUDE_CODE_AUTO_COMPACT_WINDOW\n"));
        assert!(!disabled.contains("export CLAUDE_CODE_AUTO_COMPACT_WINDOW="));
        assert!(enabled.contains("unset CLAUDE_CODE_AUTO_COMPACT_WINDOW\n"));
        assert!(!enabled.contains("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"));
        assert!(!enabled.contains("CLAUDE_CODE_EXTENDED_CACHE_TTL"));
        assert!(!disabled.contains("CLAUDE_CODE_EXTENDED_CACHE_TTL"));
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
pub(crate) fn debug_log(msg: &str) {
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
pub(crate) fn log_producer_exit(
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

/// Execute a simple Claude CLI call with `--print` flag (no tools, text-only response).
/// Used for short synchronous tasks like meeting participant selection.
/// This is a blocking function — call from tokio::task::spawn_blocking.
pub fn execute_command_simple_cancellable(
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<String, String> {
    execute_command_simple_with_model_and_cancel(prompt, None, cancel_token, None)
}

/// Cancel-aware variant of the model-override simple execution path.
///
/// Threads the supplied `CancelToken` into the spawned Claude child so that a
/// mid-flight cancel (e.g. voice barge-in) terminates the process tree
/// instead of letting it run to natural exit. Required by ADR #2175 for all
/// non-foreground entry points — call sites that hold a `CancelToken` from
/// the surrounding turn MUST use this variant.
///
/// This is a blocking function — call from `tokio::task::spawn_blocking`.
pub fn execute_command_simple_cancellable_with_model(
    prompt: &str,
    model_override: Option<&str>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<String, String> {
    let borrow = cancel_token.as_deref();
    execute_command_simple_with_model_and_cancel(
        prompt,
        model_override,
        borrow,
        cancel_token.clone(),
    )
}

fn execute_command_simple_with_model_and_cancel(
    prompt: &str,
    model_override: Option<&str>,
    cancel_token: Option<&CancelToken>,
    cancel_token_arc: Option<std::sync::Arc<CancelToken>>,
) -> Result<String, String> {
    let session_selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_capability(
            &ProviderKind::Claude,
            false,
        );
    session_selection.log_start("claude.execute_command_simple");

    let (claude_bin, resolution) = resolve_claude_binary()?;

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

    let mut builder =
        ClaudeCommandBuilder::for_binary(&claude_bin, &resolution, ClaudeLaunchIntent::Turn);
    configure_execute_command_simple(builder.command_mut(), &args);
    let mut command = builder.into_command();
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start Claude: {}", e))?;

    let child_pid = child.id();
    register_child_pid(cancel_token, child_pid);
    if cancel_requested(cancel_token) {
        kill_child_tree(&mut child);
        return Err("Claude request cancelled".to_string());
    }

    // Issue #2335 (d): arm the mid-flight cancel watcher BEFORE writing to
    // stdin. Previously the watcher was spawned after the (potentially
    // blocking) `stdin.write_all`, leaving a short window where an
    // immediate cancel arriving between `spawn` and stdin completion would
    // not be honoured. The Codex counterpart has no stdin so it is not
    // affected. ADR #2175.
    let cancel_watcher =
        crate::services::process::spawn_simple_cancel_watcher(cancel_token_arc, child_pid);

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }

    let output_result = child.wait_with_output();
    cancel_watcher.disarm();
    let was_cancelled = cancel_requested(cancel_token);
    let output = output_result.map_err(|e| format!("Failed to read output: {}", e))?;

    if was_cancelled {
        return Err("Claude request cancelled".to_string());
    }

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

fn configure_execute_command_simple(command: &mut Command, args: &[String]) {
    // Binary resolution (PATH) and the gateway launch env are applied
    // by-construction by `ClaudeCommandBuilder::for_binary`; this helper only
    // adds the non-gateway launch config.
    // #2250: put Claude in its own process group so the simple-cancel
    // watcher can terminate any wrapper / grandchild via
    // `kill_pid_tree(child_pid)`. Without this, descendants survive cancel.
    crate::services::process::configure_child_process_group(command);
    command
        .args(args)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "4096")
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Compact-window overlay (#4591): this legacy/simple entrypoint does not
    // receive the authoritative compact policy inputs, so it must never inherit
    // an absolute window from dcserver or a parent Claude process. (The gateway
    // env is applied by-construction at the `ClaudeCommandBuilder::for_binary`
    // call site, so it is not re-applied here.)
    apply_auto_compact_window_to_command(command, None);
}

// #3034: retained for the #2387 timeout-drain regression test below.
#[allow(dead_code)]
fn execute_command_simple_with_timeout_worker<F>(
    timeout: std::time::Duration,
    label: &str,
    provider_name: &'static str,
    run_worker: F,
) -> Result<String, String>
where
    F: FnOnce(std::sync::Arc<CancelToken>) -> Result<String, String> + Send + 'static,
{
    let label_owned = label.to_string();
    let cancel_token = std::sync::Arc::new(CancelToken::new());
    let cancel_for_worker = std::sync::Arc::clone(&cancel_token);
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = run_worker(std::sync::Arc::clone(&cancel_for_worker));
        *cancel_for_worker
            .child_pid
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            let _ = worker.join();
            result
        }
        Err(_) => {
            if let Ok(result) = rx.try_recv() {
                tracing::debug!(
                    provider = provider_name,
                    stage = %label_owned,
                    "execute_command_simple_with_timeout completed in race with timeout; skipping kill"
                );
                let _ = worker.join();
                return result;
            }

            tracing::warn!(
                provider = provider_name,
                stage = %label_owned,
                timeout_secs = timeout.as_secs(),
                "execute_command_simple_with_timeout timed out; cancelling and killing child"
            );
            cancel_token.cancel_with_tmux_cleanup();
            let child_pid = cancel_token
                .child_pid
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let child_pid_was_none = child_pid.is_none();
            if let Some(pid) = child_pid {
                tracing::warn!(
                    provider = provider_name,
                    stage = %label_owned,
                    child_pid = pid,
                    "execute_command_simple_with_timeout sending SIGTERM/SIGKILL to child process group"
                );
                kill_pid_tree(pid);
            } else {
                tracing::warn!(
                    provider = provider_name,
                    stage = %label_owned,
                    "execute_command_simple_with_timeout had no registered child PID at cancel time"
                );
            }

            if let Ok(result) = rx.recv_timeout(std::time::Duration::from_secs(3)) {
                let _ = worker.join();
                if child_pid_was_none {
                    tracing::debug!(
                        provider = provider_name,
                        stage = %label_owned,
                        "execute_command_simple_with_timeout drained natural result after no child PID snapshot"
                    );
                    return result;
                }
            } else {
                tracing::warn!(
                    provider = provider_name,
                    stage = %label_owned,
                    "execute_command_simple_with_timeout worker did not drain within 3s; abandoning join"
                );
            }

            Err(format!(
                "{label_owned} timeout after {}s",
                timeout.as_secs()
            ))
        }
    }
}

#[cfg(test)]
mod simple_timeout_2387_tests {
    use super::execute_command_simple_with_timeout_worker;
    use std::time::Duration;

    #[test]
    fn timeout_drain_prefers_late_result_when_child_pid_snapshot_is_none() {
        let result = execute_command_simple_with_timeout_worker(
            Duration::from_millis(10),
            "claude 2387 regression",
            "claude",
            |_cancel_token| {
                std::thread::sleep(Duration::from_millis(40));
                Ok("claude natural completion".to_string())
            },
        );

        assert_eq!(result.unwrap(), "claude natural completion");
    }
}

#[cfg(test)]
mod simple_launch_env_tests {
    use super::configure_execute_command_simple;
    use crate::services::claude_command::{ClaudeCommandBuilder, ClaudeLaunchEnv};

    fn claude_resolution() -> crate::services::platform::BinaryResolution {
        crate::services::platform::BinaryResolution {
            requested_binary: "claude".to_string(),
            resolved_path: Some("claude".to_string()),
            canonical_path: None,
            source: Some("test".to_string()),
            attempts: Vec::new(),
            failure_kind: None,
            exec_path: None,
        }
    }

    fn simple_command_env(
        launch_env: ClaudeLaunchEnv,
    ) -> std::collections::HashMap<String, Option<String>> {
        let resolution = claude_resolution();
        let binary =
            crate::services::claude_command::ClaudeBinary::from_tmux_wrapper_argv("claude");
        // Route through the chokepoint exactly as the production simple `-p`
        // spawn site does: the builder applies the gateway env by construction,
        // then `configure_execute_command_simple` adds the non-gateway config.
        let mut builder =
            ClaudeCommandBuilder::for_binary_with_env(&binary, &resolution, launch_env);
        configure_execute_command_simple(builder.command_mut(), &[]);
        builder
            .into_command()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn disabled_gateway_scrubs_pre_set_proxy_vars_from_simple_command() {
        let envs = simple_command_env(ClaudeLaunchEnv::scrub_for_test());
        assert_eq!(envs.get("ANTHROPIC_BASE_URL"), Some(&None));
        assert_eq!(
            envs.get("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some(&None)
        );
        assert_eq!(envs.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW"), Some(&None));
    }

    #[test]
    fn enabled_gateway_injects_proxy_vars_into_simple_command() {
        let envs = simple_command_env(ClaudeLaunchEnv::inject_for_test("http://127.0.0.1:10100"));
        assert_eq!(
            envs.get("ANTHROPIC_BASE_URL"),
            Some(&Some("http://127.0.0.1:10100".to_string()))
        );
        assert_eq!(
            envs.get("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some(&Some("1".to_string()))
        );
    }
}

/// Execute a command using Claude CLI with streaming output
/// If `system_prompt` is None, uses the default file manager system prompt.
/// If `system_prompt` is Some(""), no system prompt is appended.
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
    compact_lower_bound_tokens: u64,
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
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            &ProviderKind::Claude,
            entrypoint_supports_tui_hosting,
            report_channel_id,
        );
    session_selection.log_start("claude.execute_command_streaming");

    // Phase 1 of the claude-e rollout: route to the per-turn adapter
    // when the operator picked `runtime: claude-e` and the binary is
    // present. The adapter ignores tmux/remote — those routes stay on
    // their existing drivers.
    if session_selection.driver == crate::services::provider_hosting::ProviderSessionDriver::ClaudeE
    {
        debug_log("Routing to claude_e::execute_streaming (runtime=claude-e)");
        return crate::services::claude_e::execute_streaming(
            prompt,
            session_id,
            working_dir,
            sender,
            system_prompt,
            cancel_token,
            report_channel_id,
            report_provider,
            model_override,
            fast_mode_enabled,
            compact_percent,
            compact_lower_bound_tokens,
            cache_ttl_minutes,
            dispatch_type,
        );
    }
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
        let tmux_available = is_tmux_available();
        #[cfg(unix)]
        {
            if remote_profile.is_none()
                && tmux_available
                && session_selection.driver == ProviderSessionDriver::TuiHosting
            {
                if let Some(hook_endpoint) =
                    crate::services::claude_tui::hook_server::current_hook_endpoint()
                {
                    cleanup_process_backend_before_tmux(tmux_name);
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
                if tmux_available {
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
            } else if tmux_available {
                // Local with tmux → TmuxBackend (existing path)
                cleanup_process_backend_before_tmux(tmux_name);
                debug_log(&format!("TmuxBackend session: {}", tmux_name));
                return execute_streaming_local_tmux(
                    &args,
                    prompt,
                    session_id,
                    working_dir,
                    sender,
                    cancel_token,
                    tmux_name,
                    report_channel_id,
                    report_provider,
                    compact_percent,
                    compact_lower_bound_tokens,
                );
            } else {
                let (tmux_missing, pane_liveness) =
                    process_backend_demotion_guard_liveness(Some(tmux_name));
                if should_refuse_process_backend_demotion(
                    tmux_available,
                    tmux_missing,
                    pane_liveness,
                ) {
                    prepare_tmux_backend_after_refused_process_demotion(tmux_name, pane_liveness);
                    return execute_streaming_local_tmux(
                        &args,
                        prompt,
                        session_id,
                        working_dir,
                        sender,
                        cancel_token,
                        tmux_name,
                        report_channel_id,
                        report_provider,
                        compact_percent,
                        compact_lower_bound_tokens,
                    );
                }
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
                    compact_lower_bound_tokens,
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
                compact_lower_bound_tokens,
            );
        }
    }

    // Remote execution path: SSH to remote host
    if let Some(profile) = remote_profile {
        debug_log("Remote profile detected — delegating to execute_streaming_remote()");
        return execute_streaming_remote(profile, &args, prompt, working_dir, sender, cancel_token);
    }

    let (claude_bin, resolution) = resolve_claude_binary().map_err(|error| {
        debug_log("ERROR: Claude CLI not found");
        error
    })?;

    debug_log("--- Spawning claude process ---");
    debug_log("Command: resolved Claude binary");
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
    // Binary resolution (PATH) + gateway launch env applied by-construction.
    // Resolve the launch env once so the same gateway decision drives both the
    // #4591 auto-compact-window computation and the #4559 gateway guard.
    let launch_env = ClaudeLaunchEnv::resolve(ClaudeLaunchIntent::Turn);
    let auto_compact_window = launch_auto_compact_window_for_session(
        &format!(
            "claude-direct-{}",
            session_id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string)
        ),
        model_override,
        compact_percent,
        compact_lower_bound_tokens,
        launch_env.gateway_proxy_env(),
    );
    let mut builder =
        ClaudeCommandBuilder::for_binary_with_env(&claude_bin, &resolution, launch_env);
    {
        let command = builder.command_mut();
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
        // Compact-window overlay (#4591): pin the freshly resolved immutable
        // threshold on top of the chokepoint's by-construction gateway env (or
        // clear any stale inherited value when there is none).
        apply_auto_compact_window_to_command(command, auto_compact_window);
    }
    let mut command = builder.into_command();

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
    let _cancel_watchdog =
        spawn_cancel_watchdog(cancel_token.clone(), child.id(), "claude-direct-stream");

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
                if let Some(msg_obj) = json.get("message")
                    && !observe_assistant_usage(
                        msg_obj,
                        &sender,
                        AssistantUsageState {
                            last_model: &mut last_model,
                            last_call_input_tokens: &mut last_call_input_tokens,
                            last_call_cache_create_tokens: &mut last_call_cache_create_tokens,
                            last_call_cache_read_tokens: &mut last_call_cache_read_tokens,
                            cumulative_output_tokens: &mut cumulative_output_tokens,
                            saw_per_message_usage: &mut saw_per_message_usage,
                        },
                    )
                {
                    break;
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
            if !emit_status_events_from_stream_json(&json, &sender) {
                break;
            }

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
                    StreamMessage::ToolUse { name, input, .. } => {
                        let input_preview: String = input.chars().take(200).collect();
                        debug_log(&format!(
                            "  >>> ToolUse: name={}, input_preview={:?}",
                            name, input_preview
                        ));
                    }
                    StreamMessage::ToolResult {
                        content, is_error, ..
                    } => {
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
                        ..
                    } => {
                        debug_log(&format!(
                            "  >>> TaskNotification: task_id={task_id}, status={status}, kind={}, summary={summary}",
                            kind.as_str()
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
                    StreamMessage::StatusEvents { events } => {
                        debug_log(&format!("  >>> StatusEvents: {} event(s)", events.len()));
                    }
                    StreamMessage::ActiveUsageSnapshot {
                        model,
                        input_tokens,
                        cache_create_tokens,
                        cache_read_tokens,
                    } => {
                        debug_log(&format!(
                            "  >>> ActiveUsageSnapshot: model={model:?}, input={input_tokens}, cache_create={cache_create_tokens}, cache_read={cache_read_tokens}"
                        ));
                    }
                    StreamMessage::TmuxReady { .. }
                    | StreamMessage::RuntimeReady { .. }
                    | StreamMessage::ProcessReady { .. } => {
                        debug_log(
                            "  >>> TmuxReady/RuntimeReady/ProcessReady (ignored in direct execution)",
                        );
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
pub(crate) enum ClaudeFollowupResult {
    Delivered,
    RecreateSession { error: String },
    FinalizeWithNotice { error: String, notice: String },
}

const FOLLOWUP_PARTIAL_OUTPUT_NOTICE: &str = "⚠ 세션이 응답 도중 중단되었습니다. 일부 출력이 이미 전송되어 자동 재시작하지 않았습니다. 이어서 계속하려면 같은 요청을 다시 보내며 계속해 달라고 적어 주세요.";

pub(crate) fn classify_followup_result(
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

/// Stable string tag for producer-exit / zero-harvest observability (#3281).
#[cfg(unix)]
pub(crate) fn read_output_result_kind(read_result: &ReadOutputResult) -> &'static str {
    match read_result {
        ReadOutputResult::Completed { .. } => "completed",
        ReadOutputResult::SessionDied { .. } => "session_died",
        ReadOutputResult::Cancelled { .. } => "cancelled",
    }
}

/// #3281: a DELIVERED TUI turn that `Completed` without forwarding a single
/// parsed `StreamMessage` harvested nothing from its transcript window — the
/// turn's response text never entered the bridge. `Cancelled` is excluded:
/// `classify_followup_result` maps it to `Delivered` too, and a cancelled turn
/// legitimately forwards nothing.
#[cfg(unix)]
pub(crate) fn tui_delivered_zero_harvest(
    read_result: &ReadOutputResult,
    harvest: &ReadHarvestStats,
) -> bool {
    matches!(read_result, ReadOutputResult::Completed { .. }) && harvest.forwarded_messages == 0
}

/// #3281 health signal: one observability event per zero-harvest delivered TUI
/// turn, so the residual loss window (e.g. a fallback start offset pointing
/// past the response bytes) is measured in production instead of inferred.
pub(crate) fn emit_followup_restart_suppressed_notice(
    sender: &Sender<StreamMessage>,
    notice: &str,
) {
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
pub(crate) struct ClaudeTuiSessionResolution {
    pub(crate) session_id: String,
    pub(crate) transcript_path: std::path::PathBuf,
    pub(crate) resume: bool,
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

#[cfg(unix)]
pub(crate) fn fresh_claude_tui_session_resolution(
    working_dir: &std::path::Path,
    claude_home: Option<&std::path::Path>,
) -> Result<ClaudeTuiSessionResolution, String> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        working_dir,
        &session_id,
        claude_home,
    )?;
    Ok(ClaudeTuiSessionResolution {
        session_id,
        transcript_path,
        resume: false,
    })
}

#[cfg(unix)]
fn recover_claude_tui_session_resolution_from_runtime_binding(
    tmux_session_name: &str,
    requested_session_id: Option<&str>,
) -> Option<ClaudeTuiSessionResolution> {
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)?;
    if binding.runtime_kind != crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui {
        return None;
    }
    let session_id = binding
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| is_valid_session_id(value))?;
    if let Some(requested_session_id) = requested_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && requested_session_id != session_id
    {
        return None;
    }
    let transcript_path = std::path::PathBuf::from(binding.output_path);
    if !transcript_path.exists() {
        return None;
    }
    Some(ClaudeTuiSessionResolution {
        session_id: session_id.to_string(),
        transcript_path,
        resume: true,
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
pub(crate) fn claude_tui_turn_start_offset_after_timestamp(
    transcript_path: &std::path::Path,
    turn_started_at: chrono::DateTime<chrono::Utc>,
    fallback_offset: u64,
) -> u64 {
    match crate::services::claude_tui::transcript_tail::claude_transcript_timestamp_at_or_after(
        transcript_path,
        turn_started_at,
    ) {
        Ok(Some(offset)) => offset,
        Ok(None) => fallback_offset,
        Err(error) => {
            debug_log(&format!(
                "Claude TUI transcript timestamp scan failed for {}: {}; falling back to offset {}",
                transcript_path.display(),
                error,
                fallback_offset
            ));
            fallback_offset
        }
    }
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
    if let Some(channel_id) = report_channel_id {
        crate::services::tui_prompt_dedupe::register_tmux_channel(tmux_session_name, channel_id);
    }

    let turn_lock =
        crate::services::claude_tui::composer_lock::session_turn_lock(tmux_session_name);
    let _turn_guard = turn_lock.lock().unwrap_or_else(|error| error.into_inner());
    debug_log(&format!(
        "Claude TUI session turn lock acquired: {}",
        tmux_session_name
    ));

    let working_dir_path = std::path::Path::new(working_dir);
    let session_resolution =
        resolve_claude_tui_session_for_launch(working_dir_path, session_id, None)?;
    let mut resolved_session_id = session_resolution.session_id;
    let mut transcript_path = session_resolution.transcript_path;
    let mut transcript_path_string = transcript_path.display().to_string();
    let mut resume = session_resolution.resume;

    let session_exists = tmux_session_exists(tmux_session_name);
    let has_live_pane = tmux_session_has_live_pane(tmux_session_name);
    if session_exists
        && has_live_pane
        && !resume
        && let Some(recovered) = recover_claude_tui_session_resolution_from_runtime_binding(
            tmux_session_name,
            session_id,
        )
    {
        tracing::warn!(
            tmux_session_name,
            requested_session_id = session_id.unwrap_or("(none)"),
            recovered_session_id = %recovered.session_id,
            transcript_path = %recovered.transcript_path.display(),
            "recovered live Claude TUI session from runtime binding after selector/transcript lookup missed"
        );
        debug_log(&format!(
            "Claude TUI recovered live runtime binding for warm follow-up (session={}, transcript={})",
            tmux_session_name,
            recovered.transcript_path.display()
        ));
        resolved_session_id = recovered.session_id;
        transcript_path = recovered.transcript_path;
        transcript_path_string = transcript_path.display().to_string();
        resume = recovered.resume;
    }

    if session_exists && has_live_pane && resume {
        match try_claude_tui_warm_followup(
            resolved_session_id,
            transcript_path,
            transcript_path_string,
            resume,
            working_dir_path,
            prompt,
            sender.clone(),
            cancel_token.clone(),
            tmux_session_name,
            report_channel_id,
        ) {
            ClaudeTuiWarmFollowupOutcome::Terminal(result) => return result,
            ClaudeTuiWarmFollowupOutcome::Recreate(updated) => {
                resolved_session_id = updated.resolved_session_id;
                transcript_path = updated.transcript_path;
                transcript_path_string = updated.transcript_path_string;
                resume = updated.resume;
            }
        }
    } else if session_exists {
        cleanup_stale_claude_tui_session(tmux_session_name);
    }

    // Probe only after the warm-followup path has decided a fresh process is
    // required. If a proxy dies after launch, its env cannot be scrubbed from
    // the live process; this guard intentionally protects fresh launches only.
    let launch_env = ClaudeLaunchEnv::resolve(ClaudeLaunchIntent::Turn);
    if let Some(ref token) = cancel_token {
        token.bind_claude_tmux_session(tmux_session_name);
    }
    let owner_path = prepare_and_create_claude_tui_session(
        tmux_session_name,
        working_dir,
        working_dir_path,
        &resolved_session_id,
        system_prompt,
        model_override,
        hook_endpoint,
        resume,
        &launch_env,
    )?;
    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    // #3087: stamp a per-spawn nonce on the Claude-TUI DIRECT spawn path too.
    // Without it this path produces no `.spawn_nonce`, so the status-panel
    // instance key is `None` and the new-session boundary cannot be detected.
    if let Err(e) = crate::services::discord::write_spawn_nonce(tmux_session_name) {
        debug_log(&format!(
            "failed to write spawn nonce for {tmux_session_name} (claude-tui): {e}"
        ));
    }

    let _ = sender.send(StreamMessage::Init {
        session_id: resolved_session_id.clone(),
        raw_session_id: Some(resolved_session_id.clone()),
    });
    run_claude_tui_fresh_turn_and_finalize(
        &transcript_path,
        &transcript_path_string,
        sender,
        cancel_token,
        tmux_session_name,
        &resolved_session_id,
        report_channel_id,
        prompt,
        &owner_path,
    )
}

/// Dispatch a fresh Claude TUI turn and resolve its terminal outcome.
///
/// Verbatim extraction of the orchestrator's fresh-turn dispatch + completion
/// gate: skip-stale-bytes offset capture, the ready-retry fresh-turn run, and
/// the three terminal outcomes — fresh-turn start failure, `SessionDied` before
/// completion, and successful delivery (watcher handoff + producer-exit log).
/// Every original `return Ok/Err` is preserved as this fn's return value, and
/// the two failure paths perform the identical audit + exit-reason + kill +
/// owner-marker cleanup before returning `Err`. `owner_path` is the owner-marker
/// path returned by `prepare_and_create_claude_tui_session`, removed on either
/// failure path exactly as the inline block did.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn run_claude_tui_fresh_turn_and_finalize(
    transcript_path: &std::path::Path,
    transcript_path_string: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    resolved_session_id: &str,
    report_channel_id: Option<u64>,
    prompt: &str,
    owner_path: &str,
) -> Result<(), String> {
    // Skip any transcript bytes that predate this launch. Resume and fresh
    // turns both need this guard because a reused/colliding session_id can
    // leave stale JSONL on disk even when the launch is intentionally fresh.
    let fresh_turn_start_offset = claude_tui_fresh_turn_start_offset(transcript_path);
    let fresh_turn_result = run_claude_tui_fresh_turn_with_ready_retry(
        transcript_path_string,
        fresh_turn_start_offset,
        sender.clone(),
        cancel_token,
        tmux_session_name,
        resolved_session_id,
        prompt,
    );
    let (read_result, harvest, turn_read_start_offset) = match fresh_turn_result {
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
            crate::services::platform::tmux::kill_session(
                tmux_session_name,
                &format!("claude tui fresh turn failed: {}", error),
            );
            let _ = std::fs::remove_file(owner_path);
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
        crate::services::platform::tmux::kill_session(
            tmux_session_name,
            "claude tui session died before turn completion",
        );
        let _ = std::fs::remove_file(owner_path);
        return Err("claude tui session died before turn completion".to_string());
    }
    emit_claude_tui_watcher_handoff(
        &sender,
        transcript_path_string,
        tmux_session_name,
        transcript_path,
    );
    let transcript_len = std::fs::metadata(transcript_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    if tui_delivered_zero_harvest(&read_result, &harvest) {
        emit_claude_tui_zero_harvest(
            "claude_tui_zero_harvest_turn_delivered",
            report_channel_id,
            tmux_session_name,
            transcript_path_string,
            turn_read_start_offset,
            transcript_len,
        );
    }
    log_producer_exit(
        "tui_turn_delivered",
        Some(resolved_session_id),
        report_channel_id,
        // #3281: real forwarded-message count (was a hardcoded 0).
        usize::try_from(harvest.forwarded_messages).unwrap_or(usize::MAX),
        serde_json::json!({
            "tmux_session_name": tmux_session_name,
            "transcript_path": transcript_path_string,
            "assistant_text_bytes": harvest.assistant_text_bytes,
            "start_offset": turn_read_start_offset,
            "transcript_len": transcript_len,
            "read_result_kind": read_output_result_kind(&read_result),
        }),
    );
    Ok(())
}

/// Tear down a stale (no live pane) Claude TUI tmux session before recreating it.
/// Verbatim extraction of the pre-refactor `else if session_exists` branch.
#[cfg(unix)]
fn cleanup_stale_claude_tui_session(tmux_session_name: &str) {
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
    crate::services::platform::tmux::kill_session(
        tmux_session_name,
        "stale claude tui session cleanup before recreate",
    );
}

/// Prepare the Claude TUI launch script and hosted tmux session.
/// Verbatim prep/create extraction: temp cleanup, owner/runtime markers, launch script, create_session; marker `?` exits precede cleanup, later failures keep original cleanup, success returns owner path.
#[cfg(unix)]
fn prepare_and_create_claude_tui_session(
    tmux_session_name: &str,
    working_dir: &str,
    working_dir_path: &std::path::Path,
    resolved_session_id: &str,
    system_prompt: Option<&str>,
    model_override: Option<&str>,
    hook_endpoint: String,
    resume: bool,
    launch_env: &ClaudeLaunchEnv,
) -> Result<String, String> {
    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);
    write_tmux_owner_marker(tmux_session_name)?;
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux_session_name,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )?;
    let owner_path = tmux_owner_path(tmux_session_name);
    let mut prepared_session_files = None;
    let launch_result = (|| -> Result<std::process::Output, String> {
        let exe =
            std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
        let (claude_bin, _resolution) = resolve_claude_binary()?;
        let launch_config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
            tmux_session_name: tmux_session_name.to_string(),
            working_dir: working_dir_path.to_path_buf(),
            claude_bin,
            agentdesk_exe: exe,
            hook_endpoint,
            session_id: resolved_session_id.to_string(),
            system_prompt: system_prompt.map(str::to_string),
            model: model_override.map(str::to_string),
            resume,
            launch_env: launch_env.clone(),
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
    crate::services::claude_compact_context::persist_launch_provenance_to_tmux(
        tmux_session_name,
        launch_env.gateway_proxy_env(),
    );
    Ok(owner_path)
}

/// On success returns the read result, the harvest counters, and the actual
/// turn-read start offset (the timestamp-scan adjusted offset, #3281) so the
/// producer-exit log can report real values instead of a hardcoded `lines=0`.
#[cfg(unix)]
fn run_claude_tui_fresh_turn_with_ready_retry(
    transcript_path_string: &str,
    fresh_turn_start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    resolved_session_id: &str,
    prompt: &str,
) -> Result<(ReadOutputResult, ReadHarvestStats, u64), String> {
    for attempt in 1..=CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS {
        let hook_rx = crate::services::claude_tui::hook_server::subscribe_hook_events();
        let turn_started_at = chrono::Utc::now();
        match crate::services::claude_tui::input::send_fresh_prompt(
            tmux_session_name,
            prompt,
            cancel_token.as_deref(),
        ) {
            Ok(()) => {
                let hook_events_after = chrono::Utc::now();
                let start_offset = claude_tui_turn_start_offset_after_timestamp(
                    std::path::Path::new(transcript_path_string),
                    turn_started_at,
                    fresh_turn_start_offset,
                );
                return read_claude_tui_transcript_until_done(
                    transcript_path_string,
                    start_offset,
                    sender.clone(),
                    cancel_token.clone(),
                    tmux_session_name,
                    resolved_session_id,
                    hook_rx,
                    hook_events_after,
                )
                .map(|(read_result, harvest)| (read_result, harvest, start_offset));
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
    // #3889: a cold-boot stranded on the MCP-authentication-required welcome
    // screen is a terminal, operator-actionable condition — retrying just reboots
    // into the same blocked screen and burns another readiness window. It is
    // already a distinct (non-timeout) error, but guard it explicitly so the
    // retry loop can never re-enter it even if classification shifts.
    !crate::services::claude_tui::input::is_mcp_auth_required_error(error)
        && crate::services::claude_tui::input::is_prompt_ready_timeout_error(error)
        && attempt < CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS
}

#[cfg(unix)]
fn claude_tui_fresh_prompt_ready_backoff(completed_attempts: usize) -> Duration {
    let multiplier = completed_attempts.max(1).min(u32::MAX as usize) as u32;
    CLAUDE_TUI_FRESH_PROMPT_READY_BACKOFF_BASE * multiplier
}

#[cfg(unix)]
pub(crate) fn emit_claude_tui_watcher_handoff(
    sender: &Sender<StreamMessage>,
    transcript_path_string: &str,
    tmux_session_name: &str,
    transcript_path: &std::path::Path,
) {
    let last_offset = std::fs::metadata(transcript_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux_session_name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: transcript_path_string.to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset,
            relay_last_offset: None,
        },
    );
    let _ = sender.send(StreamMessage::RuntimeReady {
        handoff: RuntimeHandoff::ClaudeTui {
            transcript_path: transcript_path_string.to_string(),
            tmux_session_name: tmux_session_name.to_string(),
            last_offset,
        },
    });
}

#[cfg(unix)]
pub(crate) fn read_claude_tui_transcript_until_done(
    transcript_path: &str,
    start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    session_id: &str,
    hook_rx: tokio::sync::broadcast::Receiver<crate::services::claude_tui::hook_server::HookEvent>,
    hook_events_after: chrono::DateTime<chrono::Utc>,
) -> Result<(ReadOutputResult, ReadHarvestStats), String> {
    let stop_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_seen_for_probe = stop_seen.clone();
    let hook_rx = std::sync::Arc::new(std::sync::Mutex::new(hook_rx));
    let hook_rx_for_probe = hook_rx.clone();
    let expected_session_id = session_id.to_string();
    let expected_session_id_for_result = expected_session_id.clone();
    let tmux_name_alive = tmux_session_name.to_string();
    let transcript_path_for_ready = std::path::PathBuf::from(transcript_path);
    let probe = SessionProbe::new(
        move || tmux_session_has_live_pane(&tmux_name_alive),
        move || {
            log_claude_tui_hook_relay_failures(&expected_session_id);
            claude_tui_stop_hook_seen_or_ready_with_probe(
                &stop_seen_for_probe,
                &hook_rx_for_probe,
                &expected_session_id,
                hook_events_after,
                || claude_tui_transcript_turn_is_idle(&transcript_path_for_ready),
            )
        },
    );
    if let Some(early_result) = wait_for_claude_tui_transcript_file(
        transcript_path,
        start_offset,
        cancel_token.as_deref(),
        tmux_session_name,
    )? {
        // No transcript read happened — nothing was harvested (#3281).
        return Ok((early_result, ReadHarvestStats::default()));
    }
    let result = read_output_file_until_result_with_harvest(
        transcript_path,
        start_offset,
        sender,
        cancel_token,
        probe,
    )
    .map_err(|failure| failure.error);
    log_claude_tui_hook_relay_failures(&expected_session_id_for_result);
    result
}

#[cfg(unix)]
fn claude_tui_transcript_turn_is_idle(transcript_path: &std::path::Path) -> bool {
    crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(transcript_path)
        == crate::services::tui_turn_state::TuiTurnState::Idle
}

#[cfg(unix)]
fn wait_for_claude_tui_transcript_file(
    transcript_path: &str,
    start_offset: u64,
    cancel_token: Option<&CancelToken>,
    tmux_session_name: &str,
) -> Result<Option<ReadOutputResult>, String> {
    wait_for_claude_tui_transcript_file_inner(
        transcript_path,
        start_offset,
        cancel_token,
        tmux_session_name,
        CLAUDE_TUI_TRANSCRIPT_INITIAL_WAIT_TIMEOUT,
        || std::fs::metadata(transcript_path).is_ok(),
        || tmux_session_has_live_pane(tmux_session_name),
        || crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name),
    )
}

#[cfg(unix)]
fn wait_for_claude_tui_transcript_file_inner<Exists, Alive, Snapshot>(
    transcript_path: &str,
    start_offset: u64,
    cancel_token: Option<&CancelToken>,
    tmux_session_name: &str,
    timeout: Duration,
    mut exists: Exists,
    mut alive: Alive,
    mut snapshot: Snapshot,
) -> Result<Option<ReadOutputResult>, String>
where
    Exists: FnMut() -> bool,
    Alive: FnMut() -> bool,
    Snapshot: FnMut() -> crate::services::claude_tui::input::PromptReadinessSnapshot,
{
    if exists() {
        return Ok(None);
    }

    let started_at = std::time::Instant::now();
    let mut wait_interval = Duration::from_millis(10);
    loop {
        if cancel_requested(cancel_token) {
            return Ok(Some(ReadOutputResult::Cancelled {
                offset: start_offset,
            }));
        }
        if exists() {
            return Ok(None);
        }
        if !alive() {
            return Ok(Some(ReadOutputResult::SessionDied {
                offset: start_offset,
            }));
        }
        if started_at.elapsed() >= timeout {
            let snapshot = snapshot();
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                transcript_path = %transcript_path,
                timeout_secs = timeout.as_secs(),
                prompt_marker_detected = snapshot.prompt_marker_detected,
                prompt_draft_detected = snapshot.prompt_draft_detected,
                tmux_pane_alive = snapshot.tmux_pane_alive,
                capture_available = snapshot.capture_available,
                pane_tail = %snapshot.pane_tail,
                "claude_tui transcript file did not appear after prompt submission"
            );
            return Err(format!(
                "timeout waiting for claude tui transcript file after {}s; capture_available={}; prompt_marker_detected={}; prompt_draft_detected={}",
                timeout.as_secs(),
                snapshot.capture_available,
                snapshot.prompt_marker_detected,
                snapshot.prompt_draft_detected
            ));
        }
        std::thread::sleep(wait_interval);
        wait_interval = std::cmp::min(
            Duration::from_millis((wait_interval.as_millis() as f64 * 1.5) as u64),
            CLAUDE_TUI_TRANSCRIPT_INITIAL_WAIT_MAX_INTERVAL,
        );
    }
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
        // Preserve the original conservative behavior: a poisoned hook
        // receiver should not let a tmux prompt alone become a Stop signal.
        return false;
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

#[cfg(all(test, unix))]
mod claude_tui_ready_probe_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn ready_probe_uses_fallback_when_stop_hook_is_missing() {
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

    /// #3281 truth table: zero-harvest fires ONLY for a `Completed` read that
    /// forwarded nothing. Any forwarded message or a non-`Completed` read
    /// (`Cancelled` is also classified `Delivered` by
    /// `classify_followup_result`) must NOT fire.
    #[test]
    fn tui_delivered_zero_harvest_truth_table() {
        let zero = ReadHarvestStats::default();
        let harvested = ReadHarvestStats {
            forwarded_messages: 3,
            assistant_text_bytes: 42,
        };
        assert!(tui_delivered_zero_harvest(
            &ReadOutputResult::Completed { offset: 100 },
            &zero,
        ));
        assert!(!tui_delivered_zero_harvest(
            &ReadOutputResult::Completed { offset: 100 },
            &harvested,
        ));
        assert!(!tui_delivered_zero_harvest(
            &ReadOutputResult::Cancelled { offset: 100 },
            &zero,
        ));
        assert!(!tui_delivered_zero_harvest(
            &ReadOutputResult::SessionDied { offset: 100 },
            &zero,
        ));
    }

    #[test]
    fn claude_tui_transcript_idle_helper_uses_jsonl_turn_state() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
        )
        .unwrap();
        assert!(!claude_tui_transcript_turn_is_idle(file.path()));

        std::fs::write(
            file.path(),
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
        )
        .unwrap();
        assert!(claude_tui_transcript_turn_is_idle(file.path()));
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

    #[test]
    fn ready_probe_keeps_poisoned_hook_receiver_conservative() {
        let (_tx, rx) = tokio::sync::broadcast::channel(4);
        let hook_rx = Mutex::new(rx);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = hook_rx.lock().unwrap();
            panic!("poison hook receiver");
        }));
        let stop_seen = AtomicBool::new(false);

        assert!(!claude_tui_stop_hook_seen_or_ready_with_probe(
            &stop_seen,
            &hook_rx,
            "session-1",
            chrono::Utc::now(),
            || true
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
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    compact_percent: Option<u64>,
    compact_lower_bound_tokens: u64,
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
    let has_live_pane = tmux_session_has_live_pane(tmux_session_name);
    let resolved_output =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "jsonl");
    let resolved_input =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "input");
    // Resume id selected for this turn (`--resume <sid>` was pushed by the
    // caller when `session_id` is a valid id). Used to recognise a live pane
    // that was deliberately reused for provider-session continuity.
    let resume_session_id = session_id.filter(|sid| is_valid_session_id(sid));
    let startup_plan = classify_local_tmux_startup_plan(
        session_exists,
        has_live_pane,
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
                crate::services::platform::tmux::kill_session(
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
                crate::services::platform::tmux::kill_session(
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
    } else if startup_plan == LocalTmuxStartupPlan::RecreateStaleSession
        && should_preserve_live_reused_provider_session(resume_session_id, has_live_pane)
    {
        // Parity with the Codex (codex.rs) and Qwen (qwen.rs) guards: refuse to
        // kill a still-live pane that was deliberately selected for provider-
        // session reuse just because its wrapper I/O files are momentarily
        // missing (e.g. a dcserver restart lost /tmp, or runtime files were GC'd).
        // The classifier still labels this `RecreateStaleSession`, but a live
        // pane carrying an active `--resume` conversation must be preserved, not
        // recreated. Fail safely with an operator-visible message instead.
        tracing::warn!(
            tmux_session_name,
            session_id = resume_session_id.unwrap_or_default(),
            output_path_present = resolved_output.is_some(),
            input_path_present = resolved_input.is_some(),
            "refusing to kill live Claude tmux selected for provider-session reuse"
        );
        return Err(format!(
            "live Claude tmux session {tmux_session_name} was selected for reuse but wrapper I/O is unavailable; refusing stale cleanup/recreate"
        ));
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
        crate::services::platform::tmux::kill_session(
            tmux_session_name,
            "stale local session cleanup before recreate",
        );
    }

    // === Create new tmux session ===
    debug_log("No existing tmux session — creating new one");

    if let Some(ref token) = cancel_token {
        token.bind_claude_tmux_session(tmux_session_name);
    }

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
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux_session_name,
        crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper,
    )?;

    // Get paths
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
    let (claude_bin, resolution) = resolve_claude_binary()?;

    // Build wrapper command via script file to avoid tmux "command too long" errors.
    // The system prompt in --append-system-prompt can be thousands of chars, exceeding
    // tmux's command buffer limit when passed as a direct argument.
    let escaped_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");

    // A live warm-followup process keeps its original environment. Resolve and
    // warn only once the startup plan actually requires a fresh process.
    let launch_env = ClaudeLaunchEnv::resolve(ClaudeLaunchIntent::Turn);
    let auto_compact_window = launch_auto_compact_window_for_session(
        tmux_session_name,
        claude_model_from_args(args),
        compact_percent,
        compact_lower_bound_tokens,
        launch_env.gateway_proxy_env(),
    );
    let env_lines = build_tmux_launch_env_lines(
        resolution.exec_path.as_deref(),
        report_channel_id,
        report_provider,
        auto_compact_window,
        &launch_env,
    );

    let mut escaped_claude_bin = String::new();
    claude_bin.append_shell_escaped_to(&mut escaped_claude_bin);
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
        claude_bin = escaped_claude_bin,
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

    crate::services::claude_compact_context::persist_launch_provenance_to_tmux(
        tmux_session_name,
        launch_env.gateway_proxy_env(),
    );

    // Keep tmux session alive after process exits for post-mortem analysis
    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    // Stamp generation marker so post-restart watcher restore can detect old sessions
    let gen_marker_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
    let current_gen = crate::services::discord::runtime_store::load_generation();
    let _ = std::fs::write(&gen_marker_path, current_gen.to_string());

    // #3087: stamp a per-spawn nonce in a SEPARATE marker. The status-panel
    // session-instance key reads this nonce (unique per spawn) instead of the
    // `.generation` mtime, so a missing/duplicate mtime can never collapse two
    // distinct spawns into one instance key. Write errors are logged (not
    // silently swallowed) since a missing nonce degrades the panel-reset
    // boundary to best-effort.
    if let Err(e) = crate::services::discord::write_spawn_nonce(tmux_session_name) {
        debug_log(&format!(
            "failed to write spawn nonce for {tmux_session_name}: {e}"
        ));
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

    // Publish before the prompt is reachable; mark only after a successful flush.
    let write_result =
        submit_claude_wrapper_followup(cancel_token.as_deref(), tmux_session_name, || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(input_fifo_path)
                .map_err(|e| format!("Failed to open input FIFO: {}", e))
                .and_then(|mut fifo| {
                    writeln!(fifo, "{}", msg)
                        .map_err(|e| format!("Failed to write to input FIFO: {}", e))?;
                    fifo.flush()
                        .map_err(|e| format!("Failed to flush input FIFO: {}", e))?;
                    Ok(())
                })
        });

    if let Err(e) = write_result {
        if should_recreate_session_after_followup_fifo_error(&e) {
            debug_log(&format!("FIFO error triggers session recreation: {}", e));
            return Ok(ClaudeFollowupResult::RecreateSession { error: e });
        }
        return Err(e);
    }

    debug_log("Follow-up message sent to input FIFO");

    // Read output file from the offset
    let read_result = observe_claude_wrapper_followup(cancel_token.as_deref(), || {
        read_output_file_until_result(
            output_path,
            start_offset,
            sender.clone(),
            cancel_token.clone(),
            SessionProbe::tmux(tmux_session_name.to_string(), ProviderKind::Claude),
        )
    })?;

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
    compact_lower_bound_tokens: u64,
) -> Result<(), String> {
    use crate::services::session_backend::{ProcessBackend, SessionConfig};

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
    if process_session_available_for_followup(session_name) {
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
    let (claude_bin, resolution) = resolve_claude_binary()?;
    let mut wrapper_args = Vec::new();
    claude_bin.append_process_backend_wrapper_args(&mut wrapper_args);
    wrapper_args.extend(args.iter().map(|a| a.to_string()));

    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;

    let env_vars = resolution
        .exec_path
        .clone()
        .map(|path| vec![("PATH".to_string(), path)])
        .unwrap_or_default();
    // The follow-up path returned above when the existing process was healthy,
    // so probing here cannot emit one warning per warm turn.
    let launch_env = ClaudeLaunchEnv::resolve(ClaudeLaunchIntent::Turn);
    let auto_compact_window = launch_auto_compact_window_for_session(
        session_name,
        claude_model_from_args(args),
        compact_percent,
        compact_lower_bound_tokens,
        launch_env.gateway_proxy_env(),
    );
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
    let handle = backend.create_session_with_command_env(&config, |command| {
        // Chokepoint base (#4559): resolved gateway env + managed-launch marker
        // so the spawned `agentdesk tmux-wrapper` reconstructs this decision.
        launch_env.apply_to_managed_process_command(command);
        // Compact-window overlay (#4591).
        apply_auto_compact_window_to_command(command, auto_compact_window);
    })?;

    // Store child PID in cancel token
    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle.pid());
    }

    // Store handle for follow-up messages and protect it from tmux-takeover cleanup.
    let active_turn = insert_process_session_and_mark_active_turn(session_name.to_string(), handle);

    // Poll output file until result
    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token,
        process_session_probe(session_name),
    )?;
    drop(active_turn);

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

    let active_turn = mark_process_session_active_turn(session_name);
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
    drop(active_turn);

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
    fn live_reused_provider_session_is_preserved_when_wrapper_io_is_missing() {
        // Parity with codex.rs / qwen.rs: a live pane selected for reuse (valid,
        // non-empty resume id) must be preserved even though the classifier
        // labels it RecreateStaleSession because its wrapper I/O files are gone.
        assert!(should_preserve_live_reused_provider_session(
            Some("claude-session-1"),
            true,
        ));
        // A genuinely dead pane (no live pane) is still recreated, even with a
        // resume id — preservation must never over-protect a stale session.
        assert!(!should_preserve_live_reused_provider_session(
            Some("claude-session-1"),
            false,
        ));
        // A live pane that was NOT selected for reuse (no/blank resume id) has
        // no conversation to protect, so the normal recreate path applies.
        assert!(!should_preserve_live_reused_provider_session(None, true));
        assert!(!should_preserve_live_reused_provider_session(
            Some("  "),
            true
        ));
        assert!(!should_preserve_live_reused_provider_session(
            Some(""),
            true
        ));
    }

    #[test]
    fn issue_4113_process_demotion_guard_truth_table_pins_cached_missing_cells() {
        use crate::services::platform::tmux::PaneLiveness;

        let cases = [
            (PaneLiveness::Live, false, true),
            (PaneLiveness::Live, true, true),
            (PaneLiveness::DeadOrAbsent, false, false),
            (PaneLiveness::DeadOrAbsent, true, false),
            (PaneLiveness::ProbeError, false, true),
            (PaneLiveness::ProbeError, true, false),
        ];

        for (pane_liveness, tmux_missing, expected_refuse) in cases {
            assert_eq!(
                should_refuse_process_backend_demotion(false, tmux_missing, pane_liveness),
                expected_refuse,
                "pane_liveness={pane_liveness:?}, tmux_missing={tmux_missing}"
            );
        }

        assert!(!should_refuse_process_backend_demotion(
            true,
            false,
            PaneLiveness::Live,
        ));
    }

    #[test]
    fn issue_4113_cached_missing_probe_error_allows_process_fallback() {
        let (tmux_missing, pane_liveness) =
            backend_routing::process_backend_demotion_guard_liveness_from_cached_missing(
                true,
                Some("claude-existing-session"),
                |_| crate::services::platform::tmux::PaneLiveness::ProbeError,
            );

        assert!(tmux_missing);
        assert_eq!(
            pane_liveness,
            crate::services::platform::tmux::PaneLiveness::ProbeError
        );
        assert!(!should_refuse_process_backend_demotion(
            false,
            tmux_missing,
            pane_liveness,
        ));
    }

    #[test]
    fn issue_4113_cached_missing_without_recorded_session_skips_probe() {
        let (tmux_missing, pane_liveness) =
            backend_routing::process_backend_demotion_guard_liveness_from_cached_missing(
                true,
                None,
                |_| panic!("pane liveness must not be probed without a session name"),
            );

        assert!(tmux_missing);
        assert_eq!(
            pane_liveness,
            crate::services::platform::tmux::PaneLiveness::DeadOrAbsent
        );
        assert!(!should_refuse_process_backend_demotion(
            false,
            tmux_missing,
            pane_liveness,
        ));
    }

    #[test]
    fn issue_4113_active_process_wrapper_is_reaped_by_terminal_completion_after_tmux_skip() {
        let session_name = format!("claude-tmux-return-cleanup-{}", uuid::Uuid::new_v4());
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            session_name.clone(),
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 4113,
                alive: alive.clone(),
            },
        );
        let active_turn =
            crate::services::session_backend::mark_process_session_active_turn(&session_name);

        assert!(crate::services::session_backend::process_session_is_alive(
            &session_name
        ));
        assert!(!cleanup_process_backend_before_tmux(&session_name));

        assert!(alive.load(std::sync::atomic::Ordering::Relaxed));
        assert!(crate::services::session_backend::process_session_is_alive(
            &session_name
        ));

        alive.store(false, std::sync::atomic::Ordering::Relaxed);
        fold_read_output_result(
            ReadOutputResult::SessionDied { offset: 0 },
            |_| panic!("terminal death path must not emit ProcessReady"),
            |_| {
                crate::services::session_backend::remove_process_session(&session_name);
            },
        );
        drop(active_turn);
        assert_eq!(
            crate::services::session_backend::process_session_pid(&session_name),
            None,
            "terminal completion must remove the process wrapper registry entry"
        );
        assert!(!cleanup_process_backend_before_tmux(&session_name));
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
    fn claude_tui_handoff_uses_runtime_ready_without_fifo() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), b"{\"type\":\"assistant\"}\n").unwrap();
        let transcript_path = temp.path().display().to_string();
        let (sender, receiver) = std::sync::mpsc::channel();

        emit_claude_tui_watcher_handoff(&sender, &transcript_path, "claude-tui-test", temp.path());

        let message = receiver.recv().unwrap();
        match message {
            StreamMessage::RuntimeReady {
                handoff:
                    RuntimeHandoff::ClaudeTui {
                        transcript_path: actual_path,
                        tmux_session_name,
                        last_offset,
                    },
            } => {
                assert_eq!(actual_path, transcript_path);
                assert_eq!(tmux_session_name, "claude-tui-test");
                assert_eq!(last_offset, std::fs::metadata(temp.path()).unwrap().len());
            }
            other => panic!("expected RuntimeReady ClaudeTui, got {other:?}"),
        }
        assert!(
            receiver.try_recv().is_err(),
            "Claude direct TUI handoff must not emit legacy TmuxReady with an empty FIFO"
        );
    }

    #[test]
    fn claude_tui_runtime_binding_recovers_resume_transcript_when_cwd_lookup_missed() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let transcript = tempfile::NamedTempFile::new().expect("create transcript");
        let tmux_session_name = format!("AgentDesk-claude-recover-{}", uuid::Uuid::new_v4());
        let session_id = uuid::Uuid::new_v4().to_string();
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &tmux_session_name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: transcript.path().display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(session_id.clone()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );

        let recovered = recover_claude_tui_session_resolution_from_runtime_binding(
            &tmux_session_name,
            Some(&session_id),
        )
        .expect("recover binding");

        assert_eq!(recovered.session_id, session_id);
        assert_eq!(recovered.transcript_path, transcript.path());
        assert!(recovered.resume);
        assert!(crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux_session_name));
    }

    #[test]
    fn claude_tui_runtime_binding_recovery_rejects_mismatched_session() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let transcript = tempfile::NamedTempFile::new().expect("create transcript");
        let tmux_session_name = format!("AgentDesk-claude-reject-{}", uuid::Uuid::new_v4());
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &tmux_session_name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: transcript.path().display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );

        let requested_session_id = uuid::Uuid::new_v4().to_string();
        let recovered = recover_claude_tui_session_resolution_from_runtime_binding(
            &tmux_session_name,
            Some(&requested_session_id),
        );

        assert!(recovered.is_none());
        assert!(crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux_session_name));
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
    fn claude_tui_timestamp_start_offset_isolates_second_turn_from_stale_offset() {
        let transcript_path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            transcript_path.path(),
            concat!(
                r#"{"timestamp":"2026-05-28T00:00:00Z","type":"user","message":{"role":"user","content":[{"type":"text","text":"first prompt"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-05-28T00:00:01Z","type":"assistant","message":{"content":[{"type":"text","text":"first-hidden"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-05-28T00:00:02Z","type":"result","subtype":"success","result":"first done","session_id":"session-1"}"#,
                "\n",
                r#"{"timestamp":"2026-05-28T00:01:00Z","type":"user","message":{"role":"user","content":[{"type":"text","text":"second prompt"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-05-28T00:01:01Z","type":"assistant","message":{"content":[{"type":"text","text":"second-visible"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-05-28T00:01:02Z","type":"result","subtype":"success","result":"second done","session_id":"session-2"}"#,
                "\n",
            ),
        )
        .unwrap();
        let second_turn_started_at = chrono::DateTime::parse_from_rfc3339("2026-05-28T00:01:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let start_offset = claude_tui_turn_start_offset_after_timestamp(
            transcript_path.path(),
            second_turn_started_at,
            0,
        );
        assert!(start_offset > 0);

        let (sender, receiver) = std::sync::mpsc::channel();
        let result = read_output_file_until_result(
            transcript_path.path().to_str().unwrap(),
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
                |message| matches!(message, StreamMessage::Text { content } if content.contains("first-hidden"))
            ),
            "first turn content must not leak into second turn: {messages:?}"
        );
        assert!(
            messages.iter().any(
                |message| matches!(message, StreamMessage::Text { content } if content == "second-visible")
            ),
            "second turn text should be delivered: {messages:?}"
        );
        assert!(
            messages.iter().any(
                |message| matches!(message, StreamMessage::Done { result, session_id } if result == "second done" && session_id.as_deref() == Some("session-2"))
            ),
            "second turn result should complete the read: {messages:?}"
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
        // #3889: the terminal MCP-authentication block must never be retried —
        // rebooting just lands on the same blocked welcome screen.
        assert!(!should_retry_claude_tui_fresh_prompt_ready(
            "claude tui blocked on MCP server authentication: the Claude Code cold-boot welcome screen is waiting on MCP server authentication and is silently dropping prompt submissions; run /mcp in tmux session 'AgentDesk-ch-ad' to authenticate the server, then resend",
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
    fn claude_tui_transcript_wait_returns_ready_when_file_exists() {
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: String::new(),
        };
        let result = wait_for_claude_tui_transcript_file_inner(
            "/tmp/agentdesk-existing-transcript.jsonl",
            7,
            None,
            "claude-tui-test",
            Duration::from_millis(1),
            || true,
            || true,
            || snapshot.clone(),
        )
        .unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn claude_tui_transcript_wait_returns_session_died_before_timeout() {
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: false,
            capture_available: true,
            pane_tail: String::new(),
        };
        let result = wait_for_claude_tui_transcript_file_inner(
            "/tmp/agentdesk-missing-transcript.jsonl",
            9,
            None,
            "claude-tui-test",
            Duration::from_secs(10),
            || false,
            || false,
            || snapshot.clone(),
        )
        .unwrap();

        assert_eq!(result, Some(ReadOutputResult::SessionDied { offset: 9 }));
    }

    #[test]
    fn claude_tui_transcript_wait_respects_cancel_token() {
        let token = CancelToken::new();
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: String::new(),
        };
        let result = wait_for_claude_tui_transcript_file_inner(
            "/tmp/agentdesk-missing-transcript.jsonl",
            11,
            Some(&token),
            "claude-tui-test",
            Duration::from_secs(10),
            || false,
            || true,
            || snapshot.clone(),
        )
        .unwrap();

        assert_eq!(result, Some(ReadOutputResult::Cancelled { offset: 11 }));
    }

    #[test]
    fn claude_tui_transcript_wait_timeout_uses_tui_error_prefix() {
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "ready".to_string(),
        };
        let error = wait_for_claude_tui_transcript_file_inner(
            "/tmp/agentdesk-missing-transcript.jsonl",
            13,
            None,
            "claude-tui-test",
            Duration::ZERO,
            || false,
            || true,
            || snapshot.clone(),
        )
        .unwrap_err();

        assert!(error.starts_with("timeout waiting for claude tui transcript file"));
        assert!(crate::services::claude_tui::input::is_prompt_ready_timeout_error(&error));
    }
}

#[cfg(all(test, unix))]
mod claude_tui_session_resolution_tests {
    use super::*;
    use crate::services::claude_tui::hosting::{
        ClaudeTuiDraftRecoveryOutcome, ClaudeTuiRecreateState, ClaudeTuiStrandedPromptDraftState,
        claude_tui_followup_busy_before_submit_from_snapshot,
        claude_tui_followup_stranded_prompt_draft_state,
        claude_tui_unknown_transcript_draft_recreate_allowed, claude_tui_warm_followup_submit_plan,
    };

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
    fn stranded_draft_recreate_forces_non_resume_session() {
        let cwd = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let stale_session_id = uuid::Uuid::new_v4().to_string();
        let stale_transcript =
            crate::services::claude_tui::transcript_tail::claude_transcript_path(
                cwd.path(),
                &stale_session_id,
                Some(claude_home.path()),
            )
            .unwrap();
        std::fs::create_dir_all(stale_transcript.parent().unwrap()).unwrap();
        std::fs::write(&stale_transcript, "old transcript").unwrap();

        let resolution =
            fresh_claude_tui_session_resolution(cwd.path(), Some(claude_home.path())).unwrap();

        assert!(!resolution.resume);
        assert_ne!(resolution.session_id, stale_session_id);
        assert_ne!(resolution.transcript_path, stale_transcript);
        assert!(uuid::Uuid::parse_str(&resolution.session_id).is_ok());
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

    #[test]
    fn detects_non_busy_transcript_with_stranded_prompt_draft() {
        let transcript_dir = tempfile::tempdir().unwrap();
        let transcript_path = transcript_dir.path().join("session.jsonl");
        std::fs::write(
            &transcript_path,
            r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
        )
        .unwrap();
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "❯ stranded draft".to_string(),
        };

        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&snapshot, &transcript_path),
            Some(ClaudeTuiStrandedPromptDraftState::IdleTranscript)
        );

        let unknown_transcript_path = transcript_dir.path().join("unknown.jsonl");
        std::fs::write(&unknown_transcript_path, "not json").unwrap();
        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&snapshot, &unknown_transcript_path),
            Some(ClaudeTuiStrandedPromptDraftState::UnknownTranscript)
        );

        let busy_transcript_path = transcript_dir.path().join("busy.jsonl");
        std::fs::write(
            &busy_transcript_path,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"streaming"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&snapshot, &busy_transcript_path),
            None
        );

        let no_draft = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_draft_detected: false,
            ..snapshot.clone()
        };
        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&no_draft, &transcript_path),
            None
        );

        // U-13 A dead tmux pane must never classify as a recoverable draft —
        // there is nothing to recover when the pane is gone. Otherwise the
        // recovery path would invoke send-keys on a dead session and fail
        // with an inflight prompt forever stuck.
        let dead_pane = crate::services::claude_tui::input::PromptReadinessSnapshot {
            tmux_pane_alive: false,
            ..snapshot
        };
        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&dead_pane, &transcript_path),
            None
        );
    }

    #[test]
    fn response_residue_draft_heuristic_does_not_recreate_claude_tui() {
        let transcript_dir = tempfile::tempdir().unwrap();
        let transcript_path = transcript_dir.path().join("session.jsonl");
        std::fs::write(
            &transcript_path,
            r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
        )
        .unwrap();
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "계획만 적고 보류 — 1개\n  CLAUDE.md: 1, MCP: 2 │ Tools: 5 done".to_string(),
        };

        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&snapshot, &transcript_path),
            None
        );
    }

    #[test]
    fn idle_suggestion_prompt_does_not_recreate_claude_tui() {
        let transcript_dir = tempfile::tempdir().unwrap();
        let transcript_path = transcript_dir.path().join("session.jsonl");
        std::fs::write(
            &transcript_path,
            r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
        )
        .unwrap();
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\
✻ Worked for 2s
────────────────────────────────────────────────────────────────────────────
❯\u{00a0}좋아, 잘 동작하네
────────────────────────────────────────────────────────────────────────────
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on"
                .to_string(),
        };

        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&snapshot, &transcript_path),
            None
        );
    }

    #[test]
    fn stranded_followup_user_draft_below_finished_block_fires_recovery() {
        // #3924 (a, end-to-end): turn1 finished and turn2's `[User:]` follow-up
        // Enter was dropped, leaving it editable below the finished block under
        // idle-suggestion chrome. Previously the bare `[User:]` exclusion read
        // this as no-draft, so the recovery net never fired and the turn was
        // killed at the 120s transcript timeout. The recovery net must now
        // recognize the stranded draft (transcript Idle from the finished turn1
        // ⇒ IdleTranscript) so it can clear + resubmit instead of killing.
        let transcript_dir = tempfile::tempdir().unwrap();
        let transcript_path = transcript_dir.path().join("session.jsonl");
        std::fs::write(
            &transcript_path,
            r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
        )
        .unwrap();
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\
⏺ previous response
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
❯ [User: 0hbujang (ID: 343742347365974026)] follow-up whose Enter was dropped
─────────────────────────────────────────────────────────────────────────────
  CLAUDE.md: 1, MCP: 2 │ Tools: 4 done
  ⏵⏵ bypass permissions on"
                .to_string(),
        };

        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&snapshot, &transcript_path),
            Some(ClaudeTuiStrandedPromptDraftState::IdleTranscript)
        );
    }

    #[test]
    fn stranded_followup_user_draft_below_zero_tool_block_fires_recovery() {
        // #3924 codex re-review: the previously-MISSED shape. turn1 finished
        // having run ZERO tools (idle footer shows `Tools: 0 done`) and turn2's
        // `[User:]` follow-up Enter was DROPPED below it. The transcript is Idle
        // (turn1 completed, no in-progress turn), so the recovery net MUST fire —
        // the finished-0-tool `Tools: 0 done` footer must not be read as a running
        // turn. This is the false-negative the first fix re-introduced.
        let transcript_dir = tempfile::tempdir().unwrap();
        let transcript_path = transcript_dir.path().join("session.jsonl");
        std::fs::write(
            &transcript_path,
            r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
        )
        .unwrap();
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\
⏺ acknowledged, nothing to run
✻ Brewed for 1s
─────────────────────────────────────────────────────────────────────────────
❯ [User: 0hbujang (ID: 343742347365974026)] follow-up whose Enter was dropped
─────────────────────────────────────────────────────────────────────────────
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on"
                .to_string(),
        };

        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&snapshot, &transcript_path),
            Some(ClaudeTuiStrandedPromptDraftState::IdleTranscript)
        );
    }

    #[test]
    fn freshly_submitted_zero_tool_user_turn_is_not_recovered() {
        // #3924 codex re-review (the other direction): a `[User:]` turn that DID
        // submit and is RUNNING shares the exact pane shape (`Tools: 0 done`, no
        // `⏺` below the draft yet) as the stranded-below-0-tool case above — the
        // CAPTURE cannot tell them apart. The JSONL transcript is the authority:
        // an in-progress (assistant-streaming) turn classifies as non-Idle, so the
        // recovery net must return None and NOT clear/resubmit a live turn.
        let transcript_dir = tempfile::tempdir().unwrap();
        let transcript_path = transcript_dir.path().join("running.jsonl");
        std::fs::write(
            &transcript_path,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"streaming"}]}}"#,
        )
        .unwrap();
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\
⏺ previous response
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
❯ [User: 0hbujang (ID: 343742347365974026)] follow-up that just submitted
─────────────────────────────────────────────────────────────────────────────
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on"
                .to_string(),
        };

        assert_eq!(
            claude_tui_followup_stranded_prompt_draft_state(&snapshot, &transcript_path),
            None
        );
    }

    #[test]
    fn unknown_transcript_with_prompt_marker_and_draft_reaches_recovery() {
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "❯ stale draft".to_string(),
        };

        let result = claude_tui_followup_busy_before_submit_from_snapshot(
            snapshot,
            Some(crate::services::tui_turn_state::TuiTurnState::Unknown),
        );

        assert!(result.is_some());
    }

    #[test]
    fn unknown_transcript_quiescent_draft_can_recreate_hosted_tui() {
        let quiescent = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "⏺ marker\n\n✻ Baked for 13s\n\n❯ stale draft\n  🤖 Opus(H) │ Tools: 1 done"
                .to_string(),
        };
        assert!(claude_tui_unknown_transcript_draft_recreate_allowed(
            &quiescent
        ));

        let active = crate::services::claude_tui::input::PromptReadinessSnapshot {
            pane_tail: "❯ draft\n  Thinking... Esc to interrupt".to_string(),
            ..quiescent
        };
        assert!(!claude_tui_unknown_transcript_draft_recreate_allowed(
            &active
        ));
    }

    #[test]
    fn warm_followup_continues_after_prompt_draft_clear() {
        let normal = claude_tui_warm_followup_submit_plan(false, false);
        assert!(normal.submit_existing_session);
        assert!(normal.recheck_busy_before_submit);

        let draft_cleared = claude_tui_warm_followup_submit_plan(false, true);
        assert!(draft_cleared.submit_existing_session);
        assert!(
            !draft_cleared.recheck_busy_before_submit,
            "cleared draft must proceed to prompt submission instead of skipping warm follow-up"
        );

        let recreate = claude_tui_warm_followup_submit_plan(true, false);
        assert!(!recreate.submit_existing_session);
        assert!(recreate.recheck_busy_before_submit);
    }

    /// Seed for the #3038 S1 extraction: the session quartet carried into and
    /// back out of `recover_claude_tui_stranded_prompt_draft` via the `Proceed`
    /// arm must be the identity on the no-op fall-through path (no recreate
    /// ladder, all flags `false`). This guards the move-in/move-out contract the
    /// orchestrator relies on to rebind its session locals.
    #[test]
    fn draft_recovery_proceed_round_trips_session_quartet() {
        let session_id = uuid::Uuid::new_v4().to_string();
        let transcript_path = std::path::PathBuf::from("/tmp/agentdesk-3038-seed.jsonl");
        let transcript_path_string = transcript_path.display().to_string();
        let outcome = ClaudeTuiDraftRecoveryOutcome::Proceed {
            state: ClaudeTuiRecreateState {
                resolved_session_id: session_id.clone(),
                transcript_path: transcript_path.clone(),
                transcript_path_string: transcript_path_string.clone(),
                resume: true,
            },
            busy_waited: false,
            recreate_before_submit: false,
            prompt_draft_cleared_before_submit: false,
        };

        match outcome {
            ClaudeTuiDraftRecoveryOutcome::Proceed {
                state,
                busy_waited,
                recreate_before_submit,
                prompt_draft_cleared_before_submit,
            } => {
                assert_eq!(state.resolved_session_id, session_id);
                assert_eq!(state.transcript_path, transcript_path);
                assert_eq!(state.transcript_path_string, transcript_path_string);
                assert!(state.resume);
                assert!(!busy_waited);
                assert!(!recreate_before_submit);
                assert!(!prompt_draft_cleared_before_submit);
            }
            ClaudeTuiDraftRecoveryOutcome::Terminal(_) => {
                panic!("Proceed outcome must not destructure as Terminal");
            }
        }
    }

    /// The `Terminal` arm must forward the original early-return `Result`
    /// verbatim (the recovery block's cancellation exit returns `Ok(())`, fresh
    /// resolution failures return `Err(..)`). This pins the payload-passthrough
    /// contract the orchestrator depends on to surface the same value its
    /// inline early returns produced.
    #[test]
    fn draft_recovery_terminal_forwards_result_verbatim() {
        match ClaudeTuiDraftRecoveryOutcome::Terminal(Ok(())) {
            ClaudeTuiDraftRecoveryOutcome::Terminal(r) => assert_eq!(r, Ok(())),
            ClaudeTuiDraftRecoveryOutcome::Proceed { .. } => panic!("expected Terminal(Ok)"),
        }
        match ClaudeTuiDraftRecoveryOutcome::Terminal(Err("boom".to_string())) {
            ClaudeTuiDraftRecoveryOutcome::Terminal(r) => {
                assert_eq!(r, Err("boom".to_string()));
            }
            ClaudeTuiDraftRecoveryOutcome::Proceed { .. } => panic!("expected Terminal(Err)"),
        }
    }
}
