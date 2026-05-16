use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::services::agent_protocol::{RuntimeHandoff, StreamMessage};
use crate::services::claude;
use crate::services::claude_tui::hook_bundle::{
    HookBundleConfig, codex_hook_config_overrides, run_codex_hook_launch_self_check_with_exec_path,
};
use crate::services::claude_tui::hook_server::current_hook_endpoint;
use crate::services::discord::restart_report::{
    RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
};
use crate::services::process::{kill_child_tree, kill_pid_tree, shell_escape};
use crate::services::provider::{
    CancelToken, FollowupResult, ProviderKind, SessionProbe, cancel_requested,
    fold_read_output_result, is_readonly_tool_policy, register_child_pid,
    tmux_followup_fallback_after_read_error,
};
use crate::services::remote::RemoteProfile;
use crate::services::session_backend::{
    insert_process_session, process_session_is_alive, process_session_probe,
    read_output_file_until_result, read_output_file_until_result_tracked, remove_process_session,
    send_process_session_input, terminate_process_handle,
};
#[cfg(unix)]
use crate::services::tmux_diagnostics::{
    record_tmux_exit_reason, should_recreate_session_after_followup_fifo_error,
    tmux_session_exists, tmux_session_has_live_pane,
};

const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";
pub(crate) const CODEX_BACKGROUND_TASK_NOTIFICATION_ID: &str = "codex-background-event";
pub(crate) const CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS: &str = "completed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexRuntimeKind {
    DirectTui,
    LegacyWrapperFallback,
    ProcessBackend,
    RemoteDirect,
    RemoteTmux,
    DirectHeadless,
    SimpleHeadless,
}

impl CodexRuntimeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::DirectTui => "direct-tui",
            Self::LegacyWrapperFallback => "legacy-wrapper-fallback",
            Self::ProcessBackend => "process-backend",
            Self::RemoteDirect => "remote-direct",
            Self::RemoteTmux => "remote-tmux",
            Self::DirectHeadless => "direct-headless",
            Self::SimpleHeadless => "simple-headless",
        }
    }

    fn uses_codex_exec_json(self) -> bool {
        match self {
            Self::DirectTui | Self::RemoteDirect | Self::RemoteTmux => false,
            Self::LegacyWrapperFallback
            | Self::ProcessBackend
            | Self::DirectHeadless
            | Self::SimpleHeadless => true,
        }
    }
}

fn log_codex_runtime_kind(entrypoint: &'static str, runtime_kind: CodexRuntimeKind) {
    tracing::info!(
        provider = "codex",
        runtime_kind = runtime_kind.as_str(),
        uses_codex_exec_json = runtime_kind.uses_codex_exec_json(),
        entrypoint,
        "codex runtime kind selected"
    );
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CodexLaunchOptions {
    pub(crate) prompt: String,
    pub(crate) resume_session_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) compact_token_limit: Option<u64>,
    pub(crate) readonly_mode: bool,
    pub(crate) fast_mode_enabled: Option<bool>,
    pub(crate) goals_enabled: Option<bool>,
    pub(crate) cwd: Option<String>,
    pub(crate) add_dirs: Vec<String>,
}

impl CodexLaunchOptions {
    pub(crate) fn new(prompt: &str) -> Self {
        Self {
            prompt: prompt.to_string(),
            ..Self::default()
        }
    }

    pub(crate) fn with_resume_session_id(mut self, value: Option<&str>) -> Self {
        self.resume_session_id = clean_nonempty(value);
        self
    }

    pub(crate) fn with_model(mut self, value: Option<&str>) -> Self {
        self.model = clean_nonempty(value);
        self
    }

    pub(crate) fn with_reasoning_effort(mut self, value: Option<&str>) -> Self {
        self.reasoning_effort = clean_nonempty(value);
        self
    }

    pub(crate) fn with_compact_token_limit(mut self, value: Option<u64>) -> Self {
        self.compact_token_limit = value.filter(|limit| *limit > 0);
        self
    }

    pub(crate) fn with_readonly_mode(mut self, value: bool) -> Self {
        self.readonly_mode = value;
        self
    }

    pub(crate) fn with_fast_mode_enabled(mut self, value: Option<bool>) -> Self {
        self.fast_mode_enabled = value;
        self
    }

    pub(crate) fn with_goals_enabled(mut self, value: Option<bool>) -> Self {
        self.goals_enabled = value;
        self
    }

    pub(crate) fn with_cwd(mut self, value: Option<&str>) -> Self {
        self.cwd = clean_nonempty(value);
        self
    }

    pub(crate) fn with_add_dirs(mut self, values: &[&str]) -> Self {
        self.add_dirs = values
            .iter()
            .filter_map(|value| clean_nonempty(Some(value)))
            .collect();
        self
    }
}

fn clean_nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn codex_reasoning_effort_from_env() -> Option<String> {
    std::env::var("AGENTDESK_CODEX_REASONING_EFFORT")
        .ok()
        .and_then(|value| clean_nonempty(Some(&value)))
}

/// Public so onboarding/health-check can use the exact same resolution contract.
#[allow(dead_code)]
pub fn resolve_codex_path() -> Option<String> {
    crate::services::platform::resolve_provider_binary("codex").resolved_path
}

fn resolve_codex_binary() -> crate::services::platform::BinaryResolution {
    crate::services::platform::resolve_provider_binary("codex")
}

fn build_tmux_launch_env_lines(
    exec_path: Option<&str>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
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

    env_lines
}

fn append_feature_override_args(args: &mut Vec<String>, feature: &str, enabled: Option<bool>) {
    let Some(enabled) = enabled else {
        return;
    };

    args.push(if enabled {
        "--enable".to_string()
    } else {
        "--disable".to_string()
    });
    args.push(feature.to_string());
}

fn render_codex_wrapper_tmux_script(
    env_lines: &str,
    exe: &str,
    output_path: &str,
    input_fifo_path: &str,
    prompt_path: &str,
    working_dir: &str,
    codex_bin: &str,
    model: Option<&str>,
    session_id: Option<&str>,
    compact_token_limit: Option<u64>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
) -> String {
    let reasoning_effort = codex_reasoning_effort_from_env();
    let wrapper_args = build_codex_wrapper_cli_args(
        &CodexLaunchOptions::new("")
            .with_resume_session_id(session_id)
            .with_model(model)
            .with_reasoning_effort(reasoning_effort.as_deref())
            .with_compact_token_limit(compact_token_limit)
            .with_readonly_mode(false)
            .with_fast_mode_enabled(fast_mode_enabled)
            .with_goals_enabled(goals_enabled)
            .with_cwd(Some(working_dir)),
        codex_bin,
    )
    .into_iter()
    .map(|arg| format!(" \\\n  {}", shell_escape(&arg)))
    .collect::<String>();
    format!(
        "#!/bin/bash\n\
        {env}\
        exec {exe} codex-tmux-wrapper \\\n  \
        --output-file {output} \\\n  \
        --input-fifo {input_fifo} \\\n  \
        --prompt-file {prompt} \\\n  \
        --cwd {wd}{wrapper_args}\n",
        env = env_lines,
        exe = shell_escape(exe),
        output = shell_escape(output_path),
        input_fifo = shell_escape(input_fifo_path),
        prompt = shell_escape(prompt_path),
        wd = shell_escape(working_dir),
        wrapper_args = wrapper_args,
    )
}

fn append_codex_config_overrides(
    args: &mut Vec<String>,
    overrides: impl IntoIterator<Item = String>,
) {
    let insert_at = match args.iter().position(|arg| arg == "--") {
        Some(delimiter_index)
            if args.first().map(String::as_str) == Some("resume") && delimiter_index > 0 =>
        {
            delimiter_index - 1
        }
        Some(delimiter_index) => delimiter_index,
        None => args.len(),
    };
    let mut override_args = Vec::new();
    for override_value in overrides {
        override_args.push("-c".to_string());
        override_args.push(override_value);
    }
    args.splice(insert_at..insert_at, override_args);
}

fn codex_config_overrides(options: &CodexLaunchOptions) -> Vec<String> {
    let mut overrides = Vec::new();
    if let Some(effort) = options.reasoning_effort.as_deref() {
        overrides.push(format!("model_reasoning_effort={effort:?}"));
    }
    if let Some(limit) = options.compact_token_limit.filter(|limit| *limit > 0) {
        overrides.push(format!("model_auto_compact_token_limit={limit}"));
    }
    overrides
}

fn append_codex_common_launch_args(args: &mut Vec<String>, options: &CodexLaunchOptions) {
    append_codex_config_overrides(args, codex_config_overrides(options));
    if let Some(model) = options.model.as_deref() {
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    append_feature_override_args(args, "fast_mode", options.fast_mode_enabled);
    append_feature_override_args(args, "goals", options.goals_enabled);
    if let Some(cwd) = options.cwd.as_deref() {
        args.push("-C".to_string());
        args.push(cwd.to_string());
    }
    for add_dir in &options.add_dirs {
        args.push("--add-dir".to_string());
        args.push(add_dir.to_string());
    }
}

fn append_codex_sandbox_args(args: &mut Vec<String>, readonly_mode: bool) {
    if readonly_mode {
        args.extend(["--sandbox".to_string(), "read-only".to_string()]);
    } else {
        args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    }
}

fn base_tui_args(
    resume_session_id: Option<&str>,
    prompt: &str,
    model: Option<&str>,
    model_reasoning_effort: Option<&str>,
    readonly_mode: bool,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
) -> Vec<String> {
    let options = CodexLaunchOptions::new(prompt)
        .with_resume_session_id(resume_session_id)
        .with_model(model)
        .with_reasoning_effort(model_reasoning_effort)
        .with_readonly_mode(readonly_mode)
        .with_fast_mode_enabled(fast_mode_enabled)
        .with_goals_enabled(goals_enabled);
    build_codex_tui_args(&options)
}

pub(crate) fn build_codex_tui_args(options: &CodexLaunchOptions) -> Vec<String> {
    let mut args = Vec::new();
    if options.resume_session_id.is_some() {
        args.push("resume".to_string());
    }
    append_codex_common_launch_args(&mut args, options);
    append_codex_sandbox_args(&mut args, options.readonly_mode);
    if let Some(session_id) = options.resume_session_id.as_deref() {
        args.push(session_id.to_string());
    }
    args.extend(["--".to_string(), options.prompt.to_string()]);
    args
}

pub(crate) fn build_codex_exec_args(options: &CodexLaunchOptions) -> Vec<String> {
    let mut args = Vec::new();
    append_codex_common_launch_args(&mut args, options);
    args.push("exec".to_string());
    if let Some(existing_thread_id) = options.resume_session_id.as_deref() {
        args.push("resume".to_string());
        args.push(existing_thread_id.to_string());
    }
    args.extend(["--skip-git-repo-check".to_string(), "--json".to_string()]);
    append_codex_sandbox_args(&mut args, options.readonly_mode);
    args.extend(["--".to_string(), options.prompt.to_string()]);
    args
}

fn build_codex_wrapper_cli_args(options: &CodexLaunchOptions, codex_bin: &str) -> Vec<String> {
    let mut args = vec!["--codex-bin".to_string(), codex_bin.to_string()];
    if let Some(model) = options.model.as_deref() {
        args.push("--codex-model".to_string());
        args.push(model.to_string());
    }
    if let Some(effort) = options.reasoning_effort.as_deref() {
        args.push("--reasoning-effort".to_string());
        args.push(effort.to_string());
    }
    if let Some(session_id) = options.resume_session_id.as_deref() {
        args.push("--resume-session-id".to_string());
        args.push(session_id.to_string());
    }
    if let Some(limit) = options.compact_token_limit.filter(|limit| *limit > 0) {
        args.push("--compact-token-limit".to_string());
        args.push(limit.to_string());
    }
    for add_dir in &options.add_dirs {
        args.push("--add-dir".to_string());
        args.push(add_dir.to_string());
    }
    if let Some(enabled) = options.fast_mode_enabled {
        args.push("--fast-mode-state".to_string());
        args.push(if enabled {
            "enabled".to_string()
        } else {
            "disabled".to_string()
        });
    }
    if let Some(enabled) = options.goals_enabled {
        args.push("--goals-state".to_string());
        args.push(if enabled {
            "enabled".to_string()
        } else {
            "disabled".to_string()
        });
    }
    args
}

fn direct_tui_material_fallback_reason(options: &CodexLaunchOptions) -> Option<&'static str> {
    let _ = options;
    // The direct TUI renderer intentionally maps every current AgentDesk Codex
    // launch option to Codex-native flags/env before tmux is started. Keep this
    // guard as the single fallback decision point when a future option cannot
    // be represented safely in direct TUI mode.
    None
}

fn render_codex_tui_tmux_script(env_lines: &str, codex_bin: &str, args: &[String]) -> String {
    let rendered_args = args
        .iter()
        .map(|arg| shell_escape(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let arg_suffix = if rendered_args.is_empty() {
        String::new()
    } else {
        format!(" {rendered_args}")
    };
    format!(
        "#!/bin/bash\n\
        {env}\
        exec {codex_bin}{arg_suffix}\n",
        env = env_lines,
        codex_bin = shell_escape(codex_bin),
    )
}

#[cfg(unix)]
fn current_agentdesk_exe_for_hook_bundle() -> String {
    std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "agentdesk".to_string())
}

#[cfg(unix)]
fn prepare_codex_tui_hook_overrides(
    tmux_session_name: &str,
    session_id: Option<&str>,
    codex_bin: &str,
    exec_path: Option<&str>,
) -> Vec<String> {
    let Some(endpoint) = current_hook_endpoint() else {
        tracing::debug!(
            tmux_session_name,
            "Codex TUI hook receiver endpoint unavailable; launching without hook relays"
        );
        return Vec::new();
    };

    // Issue #2210: re-run the hook trust hash self-check against the binary
    // actually selected for this session. The startup-time check in
    // `server/mod.rs` only sees `resolve_codex_path()`; per-agent registry
    // channels / env overrides can resolve to a different binary here. The
    // launch-time check is deduped by (canonical_path, version-or-mtime) so
    // an in-place upgrade is still observed, and probes run with the same
    // PATH augmentation the launch will use so npm-shim installs probe ok.
    let _ = run_codex_hook_launch_self_check_with_exec_path(codex_bin, exec_path);

    let hook_session_id = session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(tmux_session_name);
    let config = HookBundleConfig {
        endpoint,
        provider: ProviderKind::Codex.as_str().to_string(),
        session_id: hook_session_id.to_string(),
        agentdesk_exe: current_agentdesk_exe_for_hook_bundle(),
    };
    codex_hook_config_overrides(&config)
}

fn should_reuse_existing_provider_session(
    existing_session_usable: bool,
    force_fresh_provider_session: bool,
) -> bool {
    existing_session_usable && !force_fresh_provider_session
}

#[cfg(unix)]
use crate::services::tmux_common::{tmux_owner_path, write_tmux_owner_marker};

pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    execute_command_simple_cancellable(prompt, None)
}

pub fn execute_command_simple_with_model(
    prompt: &str,
    model: Option<&str>,
) -> Result<String, String> {
    execute_command_simple_cancellable_with_options(
        prompt,
        model,
        true,
        Some(true),
        Some(false),
        None,
    )
}

/// Cancel-aware variant of [`execute_command_simple_with_model`].
///
/// Threads the supplied `CancelToken` into the spawned Codex child so that a
/// mid-flight cancel (e.g. voice barge-in) terminates the process tree
/// instead of letting it run to natural exit. Required by ADR #2175 for all
/// non-foreground entry points — call sites that hold a `CancelToken` from
/// the surrounding turn MUST use this variant.
///
/// This is a blocking function — call from `tokio::task::spawn_blocking`.
pub fn execute_command_simple_cancellable_with_model(
    prompt: &str,
    model: Option<&str>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<String, String> {
    execute_command_simple_cancellable_with_options(
        prompt,
        model,
        true,
        Some(true),
        Some(false),
        cancel_token,
    )
}

/// Execute a one-shot Codex CLI invocation with a hard timeout.
///
/// On timeout this mirrors `provider_exec::execute_simple_with_timeout`:
/// the shared [`CancelToken`] is tripped (also triggering tmux cleanup),
/// then `kill_pid_tree` is called on the registered child PID so the
/// Codex CLI and its descendants receive SIGTERM (process group first,
/// PID fallback), followed by SIGKILL after a short grace window. The
/// Codex spawn is now placed in its own process group via
/// `configure_child_process_group`, so the SIGTERM/SIGKILL reach
/// grand-descendants too. Without this the orphaned Codex CLI would
/// keep holding its working directory and rollout state long after the
/// caller has moved on (issue #2249).
///
/// PID-reuse safety: before signalling we re-check the channel for a
/// late natural completion (worker finished after `recv_timeout` returned
/// but before we got here). On a hit we skip the kill entirely so we
/// never SIGKILL a numeric PID that may already have been reaped and
/// reused by the OS. We also clear `child_pid` from the CancelToken when
/// the worker completes naturally, so any later cancel cannot fire a
/// stale PID. Thread cleanup is bounded: we only `join()` the worker
/// after observing it sent its result (or after the kill drained it),
/// never on an indefinitely blocked thread.
pub fn execute_command_simple_with_timeout(
    prompt: &str,
    timeout: Duration,
    label: &str,
) -> Result<String, String> {
    let prompt = prompt.to_string();
    let label_owned = label.to_string();
    let cancel_token = std::sync::Arc::new(CancelToken::new());
    let cancel_for_worker = std::sync::Arc::clone(&cancel_token);
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = execute_command_simple_cancellable(&prompt, Some(&cancel_for_worker));
        // Clear the registered child PID *before* sending the result.
        // The Child has already been reaped by wait_with_output() inside
        // execute_command_simple_cancellable, so the kernel may recycle
        // this PID at any moment. Clearing here prevents a late timeout
        // path (timer raced ahead of recv on a different timeline) from
        // signalling a reused, unrelated PID.
        *cancel_for_worker
            .child_pid
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            // Worker already finished and cleared child_pid; safe to join.
            let _ = worker.join();
            result
        }
        Err(_) => {
            // Re-check the channel: the worker may have sent its result in
            // the tiny window between recv_timeout returning Err and us
            // getting here. If so, take the natural completion and skip
            // the kill — child_pid was cleared by the worker, but the OS
            // could already have reused the numeric PID, so signalling it
            // would be a stray SIGKILL to an unrelated process.
            if let Ok(result) = rx.try_recv() {
                tracing::debug!(
                    provider = "codex",
                    stage = %label_owned,
                    "codex execute_command_simple_with_timeout completed in race with timeout; skipping kill"
                );
                let _ = worker.join();
                return result;
            }

            tracing::warn!(
                provider = "codex",
                stage = %label_owned,
                timeout_secs = timeout.as_secs(),
                "codex execute_command_simple_with_timeout timed out; cancelling and killing child"
            );
            cancel_token.cancel_with_tmux_cleanup();
            // Snapshot under lock and clear, so any concurrent observer
            // sees the same "no PID" state we are about to act on.
            let child_pid = cancel_token
                .child_pid
                .lock()
                .map(|mut guard| guard.take())
                .unwrap_or(None);
            if let Some(pid) = child_pid {
                tracing::warn!(
                    provider = "codex",
                    stage = %label_owned,
                    child_pid = pid,
                    "codex execute_command_simple_with_timeout sending SIGTERM/SIGKILL to child process group"
                );
                // kill_pid_tree sends SIGTERM to the process group (or
                // PID fallback), waits ~200ms, then escalates to SIGKILL
                // on the still-alive target. The Codex spawn is in its
                // own group (configure_child_process_group above), so
                // the negative-PID path reaches grand-descendants.
                kill_pid_tree(pid);
            } else {
                tracing::warn!(
                    provider = "codex",
                    stage = %label_owned,
                    "codex execute_command_simple_with_timeout had no registered child PID at cancel time"
                );
            }
            // Bounded drain: wait up to 3s for the worker to observe the
            // kill, drop its sender, and let the channel close. Only
            // join() if we actually saw the worker hand back a result;
            // otherwise drop the JoinHandle and let the OS reap the
            // thread when this process exits, rather than blocking the
            // caller forever on a stuck wait_with_output.
            if rx.recv_timeout(Duration::from_secs(3)).is_ok() {
                let _ = worker.join();
            } else {
                tracing::warn!(
                    provider = "codex",
                    stage = %label_owned,
                    "codex execute_command_simple_with_timeout worker did not drain within 3s; abandoning join"
                );
            }
            Err(format!(
                "{label_owned} timeout after {}s",
                timeout.as_secs()
            ))
        }
    }
}

pub fn execute_command_simple_cancellable(
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<String, String> {
    execute_command_simple_cancellable_borrow(prompt, None, false, None, None, cancel_token)
}

fn execute_command_simple_cancellable_with_options(
    prompt: &str,
    model: Option<&str>,
    readonly_mode: bool,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<String, String> {
    // Arc-based variant: spawn a mid-flight watcher that polls the cancel
    // token via the cloned Arc and kills the child PID tree on cancel.
    let borrow = cancel_token.as_deref();
    execute_command_simple_inner(
        prompt,
        model,
        readonly_mode,
        fast_mode_enabled,
        goals_enabled,
        borrow,
        cancel_token.clone(),
    )
}

fn execute_command_simple_cancellable_borrow(
    prompt: &str,
    model: Option<&str>,
    readonly_mode: bool,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    cancel_token: Option<&CancelToken>,
) -> Result<String, String> {
    // Borrow-only variant: keeps the existing pre-spawn race check and
    // child-pid registration, but does not spawn a mid-flight watcher
    // because we cannot promote the borrow to an `Arc` for thread-safe
    // sharing. Callers that need mid-flight cancellation must use the
    // `Arc<CancelToken>` API (e.g. `execute_command_simple_cancellable_with_model`).
    execute_command_simple_inner(
        prompt,
        model,
        readonly_mode,
        fast_mode_enabled,
        goals_enabled,
        cancel_token,
        None,
    )
}

fn execute_command_simple_inner(
    prompt: &str,
    model: Option<&str>,
    readonly_mode: bool,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    cancel_token: Option<&CancelToken>,
    cancel_token_arc: Option<std::sync::Arc<CancelToken>>,
) -> Result<String, String> {
    let session_selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_capability(
            &ProviderKind::Codex,
            false,
        );
    session_selection.log_start("codex.execute_command_simple");
    log_codex_runtime_kind(
        "codex.execute_command_simple",
        CodexRuntimeKind::SimpleHeadless,
    );

    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let working_dir_arg = working_dir.to_string_lossy().to_string();
    let args = build_codex_exec_args(
        &CodexLaunchOptions::new(prompt)
            .with_model(model)
            .with_reasoning_effort(Some("low"))
            .with_readonly_mode(readonly_mode)
            .with_fast_mode_enabled(fast_mode_enabled)
            .with_goals_enabled(goals_enabled)
            .with_cwd(Some(&working_dir_arg)),
    );

    let mut command = Command::new(&codex_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    // #2249 / #2250: put Codex in its own process group so kill_pid_tree(child_pid)
    // can SIGTERM/SIGKILL the whole descendant tree on cancel/timeout. Without
    // this, kill(-pid, ...) targets PGID = our own process group and the kill
    // falls back to the immediate child PID only — wrappers / grandchildren leak.
    crate::services::process::configure_child_process_group(&mut command);
    let mut child = command
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Codex: {}", e))?;

    let child_pid = child.id();
    register_child_pid(cancel_token, child_pid);
    if cancel_requested(cancel_token) {
        kill_child_tree(&mut child);
        return Err("Codex request cancelled".to_string());
    }

    // ADR #2175: mid-flight cancel watcher. `wait_with_output` blocks the
    // calling thread, so without a watcher the registered child PID would only
    // be killed by an external pid-aware interrupt path (e.g. tmux turn
    // bridge). Voice foreground/summary call sites do not have that external
    // path, so we spawn a lightweight thread that polls the cancel token and
    // SIGTERMs the child PID tree if a cancel arrives before natural exit.
    let cancel_watcher =
        crate::services::process::spawn_simple_cancel_watcher(cancel_token_arc, child_pid);

    let output_result = child.wait_with_output();
    cancel_watcher.disarm();
    let was_cancelled = cancel_requested(cancel_token);
    let output = output_result.map_err(|e| format!("Failed to read Codex output: {}", e))?;

    if was_cancelled {
        return Err("Codex request cancelled".to_string());
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("Codex exited with code {:?}", output.status.code())
        } else {
            stderr
        });
    }

    let mut final_text = String::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if json.get("type").and_then(|v| v.as_str()) != Some("item.completed") {
            continue;
        }
        let Some(item) = json.get("item") else {
            continue;
        };
        if item.get("type").and_then(|v| v.as_str()) != Some("agent_message") {
            continue;
        }
        let text = item
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            continue;
        }
        if !final_text.is_empty() {
            final_text.push_str("\n\n");
        }
        final_text.push_str(text);
    }

    let final_text = final_text.trim().to_string();
    if final_text.is_empty() {
        Err("Empty response from Codex".to_string())
    } else {
        Ok(final_text)
    }
}

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
    model: Option<&str>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    compact_token_limit: Option<u64>,
    force_fresh_provider_session: bool,
) -> Result<(), String> {
    #[cfg(unix)]
    let entrypoint_supports_tui_hosting =
        remote_profile.is_none() && tmux_session_name.is_some() && claude::is_tmux_available();
    #[cfg(not(unix))]
    let entrypoint_supports_tui_hosting = false;
    let session_selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_capability(
            &ProviderKind::Codex,
            entrypoint_supports_tui_hosting,
        );
    session_selection.log_start("codex.execute_command_streaming");

    let readonly_mode = is_readonly_tool_policy(allowed_tools);
    let prompt = compose_codex_prompt(prompt, system_prompt, allowed_tools);

    if let Some(profile) = remote_profile {
        // Issue #2193 — remote tmux is hard-refused regardless of the
        // remote-SSH gate. `docs/codex-remote-ssh-policy.md` lists
        // remote tmux as a non-goal: a tmux session on the remote
        // host can outlive the SSH owner (Codex keeps running after
        // the SSH session drops, with no AgentDesk-side cancel path),
        // so it needs its own ADR. Refusing it here keeps the gate's
        // PREREQUISITES_SATISFIED flip from accidentally enabling
        // remote tmux when only direct-SSH was implemented.
        #[cfg(unix)]
        {
            let requested_remote_tmux = tmux_session_name.is_some()
                && std::env::var("AGENTDESK_CODEX_REMOTE_TMUX")
                    .map(|value| {
                        let normalized = value.trim().to_ascii_lowercase();
                        normalized == "1" || normalized == "true" || normalized == "yes"
                    })
                    .unwrap_or(false);
            if requested_remote_tmux {
                tracing::warn!(
                    provider = "codex",
                    profile = %profile.name,
                    doc = "docs/codex-remote-ssh-policy.md",
                    "refusing Codex remote tmux dispatch: remote tmux is a \
                     policy non-goal and requires a separate ADR (#2193)"
                );
                return Err("Remote tmux execution is a policy non-goal (#2193). \
                     See docs/codex-remote-ssh-policy.md (non-goals)."
                    .to_string());
            }
        }
        // Issue #2193 — gate enforcement on the actual dispatch path.
        // Bootstrap already hard-fails when `remote_ssh_enabled=true`
        // and `PREREQUISITES_SATISFIED=false`, but the gate has to be
        // checked here too: a future change that wires a real
        // `services::remote` or starts populating `RemoteProfile` lists
        // would otherwise reach `execute_streaming_remote_*` without
        // the operator having opted in via `agentdesk.yaml`.
        //
        // The policy: both the runtime flag AND the compile-time
        // prerequisites constant must agree. If either is false, the
        // turn is refused before any SSH attempt.
        if !(crate::services::provider_hosting::codex_remote_ssh_enabled()
            && crate::services::codex_remote_policy::PREREQUISITES_SATISFIED)
        {
            tracing::warn!(
                provider = "codex",
                profile = %profile.name,
                doc = "docs/codex-remote-ssh-policy.md",
                "refusing Codex remote SSH dispatch: gate disabled or \
                 prerequisites not satisfied (#2193)"
            );
            return Err("Remote SSH execution is disabled by policy (#2193). \
                 See docs/codex-remote-ssh-policy.md."
                .to_string());
        }
        log_codex_runtime_kind(
            "codex.execute_command_streaming",
            CodexRuntimeKind::RemoteDirect,
        );
        return execute_streaming_remote_direct(
            profile,
            session_id,
            &prompt,
            model,
            fast_mode_enabled,
            goals_enabled,
            working_dir,
            sender,
            cancel_token,
        );
    }

    if let Some(tmux_name) = tmux_session_name {
        #[cfg(unix)]
        if claude::is_tmux_available() {
            if session_selection.driver
                == crate::services::provider_hosting::ProviderSessionDriver::TuiHosting
            {
                log_codex_runtime_kind(
                    "codex.execute_command_streaming",
                    CodexRuntimeKind::DirectTui,
                );
                return execute_streaming_local_tui_tmux(
                    &prompt,
                    session_id,
                    model,
                    fast_mode_enabled,
                    goals_enabled,
                    working_dir,
                    sender,
                    cancel_token,
                    tmux_name,
                    report_channel_id,
                    report_provider,
                    readonly_mode,
                    compact_token_limit,
                    force_fresh_provider_session,
                );
            }
            log_codex_runtime_kind(
                "codex.execute_command_streaming",
                CodexRuntimeKind::LegacyWrapperFallback,
            );
            return execute_streaming_local_tmux(
                &prompt,
                model,
                fast_mode_enabled,
                goals_enabled,
                session_id,
                working_dir,
                sender,
                cancel_token,
                tmux_name,
                report_channel_id,
                report_provider,
                compact_token_limit,
                force_fresh_provider_session,
            );
        }
        // ProcessBackend fallback for Codex (no tmux or non-unix)
        log_codex_runtime_kind(
            "codex.execute_command_streaming",
            CodexRuntimeKind::ProcessBackend,
        );
        return execute_streaming_local_process_codex(
            &prompt,
            model,
            fast_mode_enabled,
            goals_enabled,
            session_id,
            working_dir,
            sender,
            cancel_token,
            tmux_name,
            compact_token_limit,
            force_fresh_provider_session,
        );
    }

    log_codex_runtime_kind(
        "codex.execute_command_streaming",
        CodexRuntimeKind::DirectHeadless,
    );
    execute_streaming_direct(
        &prompt,
        session_id,
        model,
        fast_mode_enabled,
        goals_enabled,
        working_dir,
        sender,
        cancel_token,
        report_channel_id,
        report_provider,
        readonly_mode,
        compact_token_limit,
    )
}

fn compose_codex_prompt(
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> String {
    crate::services::provider::compose_structured_turn_prompt(prompt, system_prompt, allowed_tools)
}

fn execute_streaming_direct(
    prompt: &str,
    session_id: Option<&str>,
    model: Option<&str>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    readonly_mode: bool,
    compact_token_limit: Option<u64>,
) -> Result<(), String> {
    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;
    let args = build_codex_exec_args(
        &CodexLaunchOptions::new(prompt)
            .with_resume_session_id(session_id)
            .with_model(model)
            .with_reasoning_effort(Some("high"))
            .with_compact_token_limit(compact_token_limit)
            .with_readonly_mode(readonly_mode)
            .with_fast_mode_enabled(fast_mode_enabled)
            .with_goals_enabled(goals_enabled)
            .with_cwd(Some(working_dir)),
    );

    let mut command = Command::new(&codex_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    command
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(channel_id) = report_channel_id {
        command.env(RESTART_REPORT_CHANNEL_ENV, channel_id.to_string());
    }
    if let Some(provider) = report_provider {
        command.env(RESTART_REPORT_PROVIDER_ENV, provider.as_str());
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start Codex: {}", e))?;

    register_child_pid(cancel_token.as_deref(), child.id());
    // Race condition fix: if /stop arrived before PID was stored, kill now
    if cancel_requested(cancel_token.as_deref()) {
        kill_child_tree(&mut child);
        let _ = child.wait();
        return Ok(());
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Codex stdout".to_string())?;
    let reader = BufReader::new(stdout);

    let mut current_thread_id = session_id.map(str::to_string);
    let mut final_text = String::new();
    let mut saw_done = false;
    let started_at = std::time::Instant::now();

    for line in reader.lines() {
        if cancel_requested(cancel_token.as_deref()) {
            kill_child_tree(&mut child);
            return Ok(());
        }

        let line = match line {
            Ok(line) => line,
            Err(e) => return Err(format!("Failed to read Codex output: {}", e)),
        };

        if let Some(done) = handle_codex_json_line(
            &line,
            &sender,
            &mut current_thread_id,
            &mut final_text,
            started_at,
        )? {
            saw_done = saw_done || done;
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for Codex: {}", e))?;

    if !output.status.success() && !saw_done {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("Codex exited with code {:?}", output.status.code())
        } else {
            stderr
        };
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: String::new(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
        });
        return Ok(());
    }

    if !saw_done {
        let _ = sender.send(StreamMessage::Done {
            result: final_text,
            session_id: current_thread_id,
        });
    }

    Ok(())
}

fn execute_streaming_remote_direct(
    _profile: &RemoteProfile,
    _session_id: Option<&str>,
    _prompt: &str,
    _model: Option<&str>,
    _fast_mode_enabled: Option<bool>,
    _goals_enabled: Option<bool>,
    _working_dir: &str,
    _sender: Sender<StreamMessage>,
    _cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<(), String> {
    Err("Remote SSH execution is not available in AgentDesk".to_string())
}

#[cfg(unix)]
fn execute_streaming_remote_tmux(
    _profile: &RemoteProfile,
    _prompt: &str,
    _model: Option<&str>,
    _fast_mode_enabled: Option<bool>,
    _goals_enabled: Option<bool>,
    _working_dir: &str,
    _sender: Sender<StreamMessage>,
    _cancel_token: Option<std::sync::Arc<CancelToken>>,
    _tmux_session_name: &str,
    _report_channel_id: Option<u64>,
    _report_provider: Option<ProviderKind>,
) -> Result<(), String> {
    Err("Remote SSH tmux execution is not available in AgentDesk".to_string())
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn execute_streaming_local_tui_tmux(
    prompt: &str,
    session_id: Option<&str>,
    model: Option<&str>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    readonly_mode: bool,
    compact_token_limit: Option<u64>,
    force_fresh_provider_session: bool,
) -> Result<(), String> {
    let session_selection = crate::services::codex_tui::session::resolve_codex_tui_session(
        session_id,
        std::path::Path::new(working_dir),
        None,
        force_fresh_provider_session,
    );
    let reasoning_effort = codex_reasoning_effort_from_env();
    let launch_options = CodexLaunchOptions::new(prompt)
        .with_resume_session_id(session_selection.resume_session_id())
        .with_model(model)
        .with_reasoning_effort(reasoning_effort.as_deref())
        .with_compact_token_limit(compact_token_limit)
        .with_readonly_mode(readonly_mode)
        .with_fast_mode_enabled(fast_mode_enabled)
        .with_goals_enabled(goals_enabled)
        .with_cwd(Some(working_dir));
    if let Some(fallback_reason) = direct_tui_material_fallback_reason(&launch_options) {
        tracing::warn!(
            provider = "codex",
            unsupported_options = fallback_reason,
            tmux_session_name,
            "codex direct TUI launch options are unsupported; falling back to wrapper"
        );
        return execute_streaming_local_tmux(
            prompt,
            model,
            fast_mode_enabled,
            goals_enabled,
            session_id,
            working_dir,
            sender,
            cancel_token,
            tmux_session_name,
            report_channel_id,
            report_provider,
            compact_token_limit,
            force_fresh_provider_session,
        );
    }

    let session_exists = tmux_session_exists(tmux_session_name);
    let has_live_pane = tmux_session_has_live_pane(tmux_session_name);

    if session_exists {
        let cleanup_reason = if force_fresh_provider_session {
            "codex tui fresh provider session requested"
        } else if has_live_pane {
            "codex tui local session restart before direct launch"
        } else {
            "stale codex tui local session cleanup before recreate"
        };
        record_tmux_exit_reason(tmux_session_name, cleanup_reason);
        crate::services::platform::tmux::kill_session_with_reason(
            tmux_session_name,
            cleanup_reason,
        );
    }

    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);
    write_tmux_owner_marker(tmux_session_name)?;
    let owner_path = tmux_owner_path(tmux_session_name);

    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;
    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");
    let env_lines = build_tmux_launch_env_lines(
        resolution.exec_path.as_deref(),
        report_channel_id,
        report_provider,
    );
    let mut args = build_codex_tui_args(&launch_options);
    let codex_hook_overrides = prepare_codex_tui_hook_overrides(
        tmux_session_name,
        session_id,
        &codex_bin,
        resolution.exec_path.as_deref(),
    );
    if !codex_hook_overrides.is_empty() {
        append_codex_config_overrides(&mut args, codex_hook_overrides);
    }
    let script_content = render_codex_tui_tmux_script(&env_lines, &codex_bin, &args);
    let rollout_modified_since = std::time::SystemTime::now();

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write Codex TUI launch script: {}", e))?;

    let tmux_result = crate::services::platform::tmux::create_session(
        tmux_session_name,
        Some(working_dir),
        &format!("bash {}", shell_escape(&script_path)),
    )?;

    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        let _ = std::fs::remove_file(&owner_path);
        let _ = std::fs::remove_file(&script_path);
        return Err(format!("tmux error: {}", stderr));
    }

    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(tmux_session_name.to_string());
        if let Some(pid) = crate::services::platform::tmux::pane_pid(tmux_session_name) {
            *token.child_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
        }
    }

    // #2172 cancel boundary: keep a clone of the cancel token so that
    // post-tail emission (RuntimeReady / SessionDied Done) and the
    // tail-error tmux-cleanup branch can BOTH consult `cancel_requested`
    // without re-acquiring it. The tail call itself moves its clone into
    // the rollout-tail thread; this clone stays in the launch frame.
    let cancel_token_for_post_tail = cancel_token.clone();
    let tail_result = if session_selection.resume {
        let rollout_path = session_selection
            .rollout_path
            .as_deref()
            .ok_or_else(|| "Codex TUI resume selected without rollout path".to_string())?;
        let start_offset = session_selection.rollout_start_offset.unwrap_or(0);
        let selected_session_id = session_selection
            .selected_session_id
            .as_deref()
            .ok_or_else(|| "Codex TUI resume selected without session id".to_string())?;
        crate::services::codex_tui::rollout_tail::tail_resumed_rollout_for_session_with_handoff(
            std::path::Path::new(working_dir),
            selected_session_id,
            rollout_path,
            start_offset,
            rollout_modified_since,
            sender.clone(),
            cancel_token,
            || tmux_session_has_live_pane(tmux_session_name),
        )
    } else {
        crate::services::codex_tui::rollout_tail::tail_latest_rollout_for_cwd_with_handoff(
            std::path::Path::new(working_dir),
            rollout_modified_since,
            sender.clone(),
            cancel_token,
            || tmux_session_has_live_pane(tmux_session_name),
        )
    };
    let cancel_observed =
        || crate::services::provider::cancel_requested(cancel_token_for_post_tail.as_deref());

    let tail_result = match tail_result {
        Ok(result) => result,
        Err(error) => {
            // #2172 cancel boundary: a user /stop that arrives before the
            // rollout file is discovered surfaces as an Err from the
            // wait_for_* helpers ("cancelled waiting for Codex rollout
            // transcript").
            //
            // Two consequences must be suppressed:
            //   (a) Killing the tmux session here contradicts the
            //       PreserveSession default for /stop — the pane
            //       teardown is owned by stop_active_turn's
            //       TmuxCleanupPolicy.
            //   (b) Returning Err is converted into
            //       `StreamMessage::Error` by the streaming-launch
            //       caller (router/message_handler.rs spawn_blocking
            //       wrapper) and would reach the bridge as a transport
            //       error instead of a cancelled turn — letting the
            //       bridge run its error-finalisation path on a
            //       cancelled turn.
            //
            // Return Ok(()) with no StreamMessage emitted: the producer
            // is silent post-cancel and the bridge's cancel arm drives
            // finalisation.
            if cancel_observed() {
                tracing::info!(
                    tmux_session = tmux_session_name,
                    error = %error,
                    "Codex rollout tail cancelled before transcript; suppressing tail Err and deferring tmux cleanup to cancel path"
                );
                return Ok(());
            }
            // #2182 follow-up: rollout wait / tail failures used to leak the
            // tmux session because `?` propagated Err without cleaning the
            // launched session. Kill it explicitly so the worktree doesn't
            // accumulate dangling Codex TUIs.
            tracing::warn!(
                tmux_session = tmux_session_name,
                error = %error,
                "Codex rollout tail failed; killing tmux session to avoid leak"
            );
            record_tmux_exit_reason(tmux_session_name, &format!("rollout tail failed: {error}"));
            crate::services::platform::tmux::kill_session_with_reason(
                tmux_session_name,
                "codex rollout tail failed",
            );
            return Err(error);
        }
    };

    let read_result = tail_result.read_result.clone();
    // #2172 cancel boundary: relay suppression is enforced at every
    // Direct TUI StreamMessage producer, not just rollout_tail. The
    // post-tail SessionDied Done and the RuntimeReady handoff frame
    // must also drop on the floor when the user has cancelled — a
    // cancelled turn must not deliver any further frame to the bridge.
    // ReadOutputResult::Cancelled is handled explicitly: it never
    // emits RuntimeReady (which would let the bridge mutate handoff
    // state on a cancelled turn) and it never emits Done either.
    if matches!(
        read_result,
        crate::services::provider::ReadOutputResult::Cancelled { .. }
    ) {
        tracing::info!(
            tmux_session = tmux_session_name,
            "Codex Direct TUI tail returned Cancelled; suppressing post-tail StreamMessage emission"
        );
        return Ok(());
    }
    if cancel_observed() {
        tracing::info!(
            tmux_session = tmux_session_name,
            "Codex Direct TUI launch observed cancel after tail returned; suppressing post-tail StreamMessage emission"
        );
        return Ok(());
    }
    if matches!(
        read_result,
        crate::services::provider::ReadOutputResult::SessionDied { .. }
    ) {
        let _ = sender.send(StreamMessage::Done {
            result: "⚠ Codex TUI session ended before producing a response.".to_string(),
            session_id: None,
        });
    } else {
        // #2325: gate the RuntimeReady handoff on the Codex TUI composer
        // actually being ready for input. RuntimeReady is the signal the
        // turn-bridge uses to publish CodexTui handoff state that
        // downstream recovery / watcher-relay paths assume corresponds
        // to a live, input-ready pane (see
        // `services::discord::turn_bridge::mod::RuntimeHandoff::CodexTui`
        // branch). If we publish RuntimeReady against a tmux session
        // whose composer never came back up, downstream consumers will
        // operate on a non-ready handoff.
        //
        // Bridge-drain race (Codex round-3 review on #2325):
        // `rollout_tail` has already emitted `StreamMessage::Done` by
        // the time we get here, so the bridge has started its
        // `terminal_control_drain_until` window (250ms) before it
        // finalises the inflight. The readiness wait MUST fit inside
        // that window or our `RuntimeReady` / failure `Done` will be
        // dropped after the bridge has already cleared inflight state.
        // We use `PromptReadinessKind::PostTurnHandoff` (200ms budget)
        // and split outcomes:
        //   - Ready → emit RuntimeReady (handoff preserved).
        //   - Session dead → emit failure Done; tmux death is
        //     observable synchronously so the verdict reaches the
        //     bridge inside the drain window.
        //   - Composer not yet redrawn within the probe budget → emit
        //     RuntimeReady anyway with a tracing warning. The
        //     assistant response has already shipped via the tail
        //     `Done`; preserving the handoff is the safe default for
        //     recovery / watcher-relay even if the visual composer is
        //     still settling. Making this case hard-fail would require
        //     cross-bridge cooperation tracked separately.
        match crate::services::codex_tui::input::wait_until_codex_tui_input_ready(
            tmux_session_name,
            crate::services::codex_tui::input::PromptReadinessKind::PostTurnHandoff,
            cancel_token_for_post_tail.as_ref(),
        ) {
            Ok(()) => {
                let _ = sender.send(StreamMessage::RuntimeReady {
                    handoff: RuntimeHandoff::CodexTui {
                        rollout_path: tail_result.rollout_path.display().to_string(),
                        thread_id: tail_result.session_id,
                        tmux_session_name: tmux_session_name.to_string(),
                        last_offset: tail_result.final_offset,
                    },
                });
            }
            Err(error)
                if crate::services::codex_tui::input::is_prompt_ready_cancelled_error(&error) =>
            {
                // Cancel beats deadline / session-death — match the
                // post-tail cancel-suppression behaviour above: emit no
                // further StreamMessage and let the bridge's cancel arm
                // drive finalisation.
                tracing::info!(
                    tmux_session = tmux_session_name,
                    "Codex TUI input readiness wait cancelled post-turn; suppressing RuntimeReady"
                );
                return Ok(());
            }
            Err(error) if crate::services::codex_tui::input::is_session_dead_error(&error) => {
                // Session death is detected synchronously by the tmux
                // pane-alive check, so this verdict reaches the bridge
                // inside its drain window. Skip RuntimeReady and surface
                // a failure Done.
                tracing::warn!(
                    tmux_session = tmux_session_name,
                    error = %error,
                    "Codex TUI session died before becoming input-ready; suppressing RuntimeReady"
                );
                let _ = sender.send(StreamMessage::Done {
                    result: "⚠ Codex TUI session ended before becoming input-ready.".to_string(),
                    session_id: tail_result.session_id.clone(),
                });
            }
            Err(error) => {
                // Composer did not redraw within the 200ms probe budget.
                // The assistant response already shipped, the pane is
                // still alive — preserve the handoff so recovery /
                // watcher-relay paths remain functional, and record a
                // warning for telemetry. Making this case hard-fail
                // would require the bridge to keep its inflight open
                // beyond the 250ms drain (tracked as a follow-up to
                // #2325).
                tracing::warn!(
                    tmux_session = tmux_session_name,
                    error = %error,
                    "Codex TUI composer not yet input-ready inside post-turn probe budget; emitting RuntimeReady best-effort"
                );
                let _ = sender.send(StreamMessage::RuntimeReady {
                    handoff: RuntimeHandoff::CodexTui {
                        rollout_path: tail_result.rollout_path.display().to_string(),
                        thread_id: tail_result.session_id,
                        tmux_session_name: tmux_session_name.to_string(),
                        last_offset: tail_result.final_offset,
                    },
                });
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
fn execute_streaming_local_tmux(
    prompt: &str,
    model: Option<&str>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    compact_token_limit: Option<u64>,
    force_fresh_provider_session: bool,
) -> Result<(), String> {
    let output_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let input_fifo_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "input");
    let prompt_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "prompt");
    let owner_path = tmux_owner_path(tmux_session_name);

    // Accept either the new persistent location or the legacy /tmp location
    // so that dcserver restarts that lost /tmp files still re-attach to a
    // live tmux pane owned by an older wrapper. See issue #892.
    let session_exists = tmux_session_exists(tmux_session_name);
    let resolved_output =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "jsonl");
    let resolved_input =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "input");
    let session_usable = tmux_session_has_live_pane(tmux_session_name)
        && resolved_output.is_some()
        && resolved_input.is_some();

    if should_reuse_existing_provider_session(session_usable, force_fresh_provider_session) {
        let output_path = resolved_output
            .clone()
            .unwrap_or_else(|| output_path.clone());
        let input_fifo_path = resolved_input
            .clone()
            .unwrap_or_else(|| input_fifo_path.clone());
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
                crate::services::platform::tmux::kill_session_with_reason(
                    tmux_session_name,
                    &format!("followup failed, recreating: {}", error),
                );
                // Fall through to new session creation below
            }
        }
    } else if session_exists {
        let cleanup_reason = if force_fresh_provider_session {
            "codex fresh provider session requested"
        } else {
            "stale local session cleanup before recreate"
        };
        record_tmux_exit_reason(tmux_session_name, cleanup_reason);
        crate::services::platform::tmux::kill_session_with_reason(
            tmux_session_name,
            cleanup_reason,
        );
    }

    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);

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
    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;

    // Write launch script to file to avoid tmux "command too long" errors
    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");

    let env_lines = build_tmux_launch_env_lines(
        resolution.exec_path.as_deref(),
        report_channel_id,
        report_provider,
    );

    let script_content = render_codex_wrapper_tmux_script(
        &env_lines,
        &exe.display().to_string(),
        &output_path,
        &input_fifo_path,
        &prompt_path,
        working_dir,
        &codex_bin,
        model,
        session_id,
        compact_token_limit,
        fast_mode_enabled,
        goals_enabled,
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

    // Keep tmux session alive after process exits for post-mortem analysis
    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    // Stamp generation marker so post-restart watcher restore can detect old sessions
    let gen_marker_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
    let current_gen = crate::services::discord::runtime_store::load_generation();
    let _ = std::fs::write(&gen_marker_path, current_gen.to_string());

    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(tmux_session_name.to_string());
    }

    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token.clone(),
        SessionProbe::tmux(tmux_session_name.to_string()),
    )?;

    fold_read_output_result(
        read_result,
        |offset| {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        },
        |_| {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 세션이 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
        },
    );

    Ok(())
}

#[cfg(unix)]
fn send_followup_to_tmux(
    prompt: &str,
    output_path: &str,
    input_fifo_path: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<FollowupResult, String> {
    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    // Write to input FIFO — if the pipe is broken or missing, request recreation
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
        *token.tmux_session.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(tmux_session_name.to_string());
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
                    "  [{ts}] ⚠ codex follow-up read failed for {tmux_session_name}: {}; attaching fallback watcher at offset {} (ready_for_input={}, emit_done={})",
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
                    "  [{ts}] ⚠ codex follow-up read failed and tmux session died for {tmux_session_name}: {}; recreating session",
                    failure.error
                );
                return Ok(FollowupResult::RecreateSession {
                    error: failure.error,
                });
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::error!(
                "  [{ts}] ✗ codex follow-up read failed with no watcher fallback for {tmux_session_name}: {} (output_exists={}, input_exists={})",
                failure.error,
                output_exists,
                input_exists
            );
            return Err(failure.error);
        }
    };

    Ok(fold_read_output_result(
        read_result,
        |offset| {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path: output_path.to_string(),
                input_fifo_path: input_fifo_path.to_string(),
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
            FollowupResult::Delivered
        },
        |_| FollowupResult::RecreateSession {
            error: "session died during follow-up output reading".to_string(),
        },
    ))
}

/// Execute Codex via ProcessBackend (direct child process, no tmux).
fn execute_streaming_local_process_codex(
    prompt: &str,
    model: Option<&str>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    session_name: &str,
    compact_token_limit: Option<u64>,
    force_fresh_provider_session: bool,
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

    // Check for existing process session
    let process_session_alive = process_session_is_alive(session_name);
    if should_reuse_existing_provider_session(process_session_alive, force_fresh_provider_session) {
        // Snapshot file length BEFORE sending input to avoid race:
        // Codex wrapper appends JSONL immediately on stdin, so a fast
        // response could be written before we read the offset.
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

        fold_read_output_result(
            read_result,
            |offset| {
                let _ = sender.send(StreamMessage::ProcessReady {
                    output_path: output_path.to_string(),
                    session_name: session_name.to_string(),
                    last_offset: offset,
                });
            },
            |_| {
                let _ = sender.send(StreamMessage::Done {
                    result: "⚠ 세션이 종료되었습니다.".to_string(),
                    session_id: None,
                });
                remove_process_session(session_name);
            },
        );
        return Ok(());
    }

    if force_fresh_provider_session && process_session_alive {
        if let Some(handle) = remove_process_session(session_name) {
            terminate_process_handle(handle);
        }
    }

    // Clean up and create new session
    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&prompt_path);
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;

    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
    let launch_options = CodexLaunchOptions::new(prompt)
        .with_resume_session_id(session_id)
        .with_model(model)
        .with_reasoning_effort(codex_reasoning_effort_from_env().as_deref())
        .with_compact_token_limit(compact_token_limit)
        .with_readonly_mode(false)
        .with_fast_mode_enabled(fast_mode_enabled)
        .with_goals_enabled(goals_enabled)
        .with_cwd(Some(working_dir));

    let config = SessionConfig {
        session_name: session_name.to_string(),
        working_dir: working_dir.to_string(),
        agentdesk_exe: exe.display().to_string(),
        output_path: output_path.clone(),
        prompt_path: prompt_path.clone(),
        wrapper_subcommand: "codex-tmux-wrapper".to_string(),
        wrapper_args: build_codex_wrapper_cli_args(&launch_options, &codex_bin),
        env_vars: resolution
            .exec_path
            .clone()
            .map(|path| vec![("PATH".to_string(), path)])
            .unwrap_or_default(),
    };

    let backend = ProcessBackend::new();
    let handle = backend.create_session(&config)?;

    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle.pid());
    }

    insert_process_session(session_name.to_string(), handle);

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
                result: "⚠ 프로세스가 종료되었습니다.".to_string(),
                session_id: None,
            });
            remove_process_session(session_name);
        },
    );

    Ok(())
}

fn base_exec_args(
    session_id: Option<&str>,
    prompt: &str,
    model: Option<&str>,
    model_reasoning_effort: Option<&str>,
    readonly_mode: bool,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
) -> Vec<String> {
    build_codex_exec_args(
        &CodexLaunchOptions::new(prompt)
            .with_resume_session_id(session_id)
            .with_model(model)
            .with_reasoning_effort(model_reasoning_effort)
            .with_readonly_mode(readonly_mode)
            .with_fast_mode_enabled(fast_mode_enabled)
            .with_goals_enabled(goals_enabled),
    )
}

fn normalize_codex_mcp_segment(value: &str) -> Option<String> {
    let normalized = value
        .trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn codex_mcp_invocation(json: &Value) -> Option<&Value> {
    json.get("invocation")
        .or_else(|| json.get("item").and_then(|item| item.get("invocation")))
}

fn codex_mcp_tool_name(invocation: &Value) -> Option<String> {
    let server = invocation.get("server").and_then(Value::as_str)?;
    let tool = invocation.get("tool").and_then(Value::as_str)?;
    Some(format!(
        "mcp__{}__{}",
        normalize_codex_mcp_segment(server)?,
        normalize_codex_mcp_segment(tool)?,
    ))
}

fn codex_mcp_arguments(invocation: &Value) -> String {
    match invocation.get("arguments") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|value| serde_json::to_string(&value).ok())
            .unwrap_or_else(|| Value::String(text.clone()).to_string()),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
    }
}

fn codex_mcp_error_text(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default())
}

fn codex_mcp_payload_content(payload: &Value) -> String {
    if let Some(structured) = payload
        .get("structuredContent")
        .or_else(|| payload.get("structured_content"))
        .filter(|value| !value.is_null())
    {
        return serde_json::to_string(structured).unwrap_or_default();
    }

    if let Some(content_items) = payload.get("content").and_then(Value::as_array) {
        let text_items = content_items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        if text_items.len() == 1 && serde_json::from_str::<Value>(&text_items[0]).is_ok() {
            return text_items[0].clone();
        }
        if !text_items.is_empty() {
            return text_items.join("\n\n");
        }
        if !content_items.is_empty() {
            return serde_json::to_string(content_items).unwrap_or_default();
        }
    }

    serde_json::to_string(payload).unwrap_or_default()
}

fn codex_mcp_result(result: &Value) -> (String, bool) {
    if let Some(error) = result.get("Err").or_else(|| result.get("err")) {
        return (codex_mcp_error_text(error), true);
    }

    let payload = result
        .get("Ok")
        .or_else(|| result.get("ok"))
        .unwrap_or(result);
    let is_error = payload
        .get("isError")
        .or_else(|| payload.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (codex_mcp_payload_content(payload), is_error)
}

pub(crate) fn codex_background_event_summary(json: &Value) -> Option<&str> {
    if json.get("type").and_then(Value::as_str) != Some("background_event") {
        return None;
    }

    json.get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
}

fn handle_codex_json_line(
    line: &str,
    sender: &Sender<StreamMessage>,
    current_thread_id: &mut Option<String>,
    final_text: &mut String,
    started_at: std::time::Instant,
) -> Result<Option<bool>, String> {
    if line.trim().is_empty() {
        return Ok(None);
    }

    let json = serde_json::from_str::<Value>(line)
        .map_err(|e| format!("Failed to parse Codex JSON: {}", e))?;

    match json.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "thread.started" => {
            if let Some(thread_id) = json.get("thread_id").and_then(|v| v.as_str()) {
                *current_thread_id = Some(thread_id.to_string());
                let _ = sender.send(StreamMessage::Init {
                    session_id: thread_id.to_string(),
                    raw_session_id: None,
                });
            }
        }
        "item.started" => {
            if let Some(item) = json.get("item") {
                match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "command_execution" => {
                        let command = item.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        let input = serde_json::json!({ "command": command }).to_string();
                        let _ = sender.send(StreamMessage::ToolUse {
                            name: "Bash".to_string(),
                            input,
                        });
                    }
                    "reasoning" => {
                        let _ = sender.send(StreamMessage::redacted_thinking());
                    }
                    _ => {}
                }
            }
        }
        "mcp_tool_call_begin" => {
            if let Some(invocation) = codex_mcp_invocation(&json)
                && let Some(name) = codex_mcp_tool_name(invocation)
            {
                let _ = sender.send(StreamMessage::ToolUse {
                    name,
                    input: codex_mcp_arguments(invocation),
                });
            }
        }
        "mcp_tool_call_end" => {
            let (content, is_error) = codex_mcp_result(json.get("result").unwrap_or(&Value::Null));
            let _ = sender.send(StreamMessage::ToolResult { content, is_error });
        }
        "background_event" => {
            if let Some(summary) = codex_background_event_summary(&json) {
                let _ = sender.send(StreamMessage::TaskNotification {
                    task_id: CODEX_BACKGROUND_TASK_NOTIFICATION_ID.to_string(),
                    status: CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS.to_string(),
                    summary: summary.to_string(),
                    kind: crate::services::agent_protocol::TaskNotificationKind::Background,
                });
            }
        }
        "item.completed" => {
            if let Some(item) = json.get("item") {
                match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "agent_message" => {
                        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            if !final_text.is_empty() {
                                final_text.push_str("\n\n");
                            }
                            final_text.push_str(text);
                            let _ = sender.send(StreamMessage::Text {
                                content: text.to_string(),
                            });
                        }
                    }
                    "command_execution" => {
                        let content = item
                            .get("aggregated_output")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let is_error = item
                            .get("exit_code")
                            .and_then(|v| v.as_i64())
                            .map(|code| code != 0)
                            .unwrap_or(false);
                        let _ = sender.send(StreamMessage::ToolResult { content, is_error });
                    }
                    "reasoning" => {
                        let _ = sender.send(StreamMessage::redacted_thinking());
                    }
                    _ => {}
                }
            }
        }
        "turn.completed" => {
            let usage = json.get("usage").cloned().unwrap_or_default();
            let input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64());
            let output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64());
            let _ = sender.send(StreamMessage::StatusUpdate {
                model: Some("codex".to_string()),
                cost_usd: None,
                total_cost_usd: None,
                duration_ms: Some(started_at.elapsed().as_millis() as u64),
                num_turns: None,
                input_tokens,
                cache_create_tokens: None,
                cache_read_tokens: None,
                output_tokens,
            });
            let _ = sender.send(StreamMessage::Done {
                result: final_text.clone(),
                session_id: current_thread_id.clone(),
            });
            return Ok(Some(true));
        }
        "error" => {
            let message = json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Codex error");
            let _ = sender.send(StreamMessage::Error {
                message: message.to_string(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            return Ok(Some(true));
        }
        _ => {}
    }

    Ok(Some(false))
}

#[cfg(test)]
mod tui_hosting_tests {
    use super::{
        CodexLaunchOptions, CodexRuntimeKind, append_codex_config_overrides, base_tui_args,
        build_codex_tui_args, build_tmux_launch_env_lines, direct_tui_material_fallback_reason,
        render_codex_tui_tmux_script, render_codex_wrapper_tmux_script,
    };
    use crate::services::discord::restart_report::{
        RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn codex_tui_tmux_script_launches_codex_directly_without_wrapper() {
        let args = base_tui_args(
            None,
            "hello from tui",
            Some("gpt-5-codex"),
            Some("medium"),
            false,
            Some(true),
            Some(false),
        );
        let script = render_codex_tui_tmux_script("unset CLAUDECODE\n", "/opt/bin/codex", &args);

        assert!(script.contains("exec '/opt/bin/codex' "));
        assert!(!script.contains("codex-tmux-wrapper"));
        assert!(!script.contains("'resume'"));
        assert!(script.contains("'-m' 'gpt-5-codex'"));
        assert!(script.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(script.contains("'--' 'hello from tui'"));
    }

    #[test]
    fn codex_tui_args_snapshot_preserves_common_launch_options() {
        let args = build_codex_tui_args(
            &CodexLaunchOptions::new("prompt that starts --flag")
                .with_resume_session_id(Some("session-123"))
                .with_model(Some("gpt-5-codex"))
                .with_reasoning_effort(Some("xhigh"))
                .with_compact_token_limit(Some(120_000))
                .with_readonly_mode(true)
                .with_fast_mode_enabled(Some(false))
                .with_goals_enabled(Some(true))
                .with_cwd(Some("/work/repo"))
                .with_add_dirs(&["/work/shared", "  /work/second  "]),
        );

        assert_eq!(
            args,
            vec![
                "resume",
                "-c",
                r#"model_reasoning_effort="xhigh""#,
                "-c",
                "model_auto_compact_token_limit=120000",
                "-m",
                "gpt-5-codex",
                "--disable",
                "fast_mode",
                "--enable",
                "goals",
                "-C",
                "/work/repo",
                "--add-dir",
                "/work/shared",
                "--add-dir",
                "/work/second",
                "--sandbox",
                "read-only",
                "session-123",
                "--",
                "prompt that starts --flag",
            ]
        );
    }

    #[test]
    fn codex_tui_script_snapshot_preserves_env_and_command_shape() {
        let env_lines = build_tmux_launch_env_lines(
            Some("/opt/codex/bin:/usr/bin"),
            Some(42),
            Some(ProviderKind::Codex),
        );
        let args = build_codex_tui_args(
            &CodexLaunchOptions::new("fresh prompt")
                .with_model(Some("gpt-5-codex"))
                .with_reasoning_effort(Some("medium"))
                .with_compact_token_limit(Some(64_000))
                .with_readonly_mode(false)
                .with_cwd(Some("/work/repo")),
        );
        let script = render_codex_tui_tmux_script(&env_lines, "/opt/bin/codex", &args);

        assert!(script.contains("unset CLAUDECODE\n"));
        assert!(script.contains("export PATH='/opt/codex/bin:/usr/bin'\n"));
        assert!(script.contains(&format!("export {RESTART_REPORT_CHANNEL_ENV}=42\n")));
        assert!(script.contains(&format!("export {RESTART_REPORT_PROVIDER_ENV}=codex\n")));
        if std::env::var("AGENTDESK_ROOT_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .is_some()
        {
            assert!(script.contains("export AGENTDESK_ROOT_DIR="));
        }
        assert!(script.ends_with("exec '/opt/bin/codex' '-c' 'model_reasoning_effort=\"medium\"' '-c' 'model_auto_compact_token_limit=64000' '-m' 'gpt-5-codex' '-C' '/work/repo' '--dangerously-bypass-approvals-and-sandbox' '--' 'fresh prompt'\n"));
    }

    #[test]
    fn codex_direct_tui_declares_all_current_options_supported() {
        let options = CodexLaunchOptions::new("prompt")
            .with_resume_session_id(Some("session-123"))
            .with_model(Some("gpt-5-codex"))
            .with_reasoning_effort(Some("high"))
            .with_compact_token_limit(Some(120_000))
            .with_readonly_mode(false)
            .with_fast_mode_enabled(Some(true))
            .with_goals_enabled(Some(false))
            .with_cwd(Some("/work/repo"))
            .with_add_dirs(&["/work/shared"]);

        assert_eq!(direct_tui_material_fallback_reason(&options), None);
    }

    #[test]
    fn codex_tui_fresh_args_do_not_include_resume() {
        let args = base_tui_args(None, "fresh prompt", None, None, true, None, None);

        assert!(!args.iter().any(|arg| arg == "resume"));
        assert_eq!(args.last().map(String::as_str), Some("fresh prompt"));
    }

    #[test]
    fn codex_tui_resume_args_include_session_id_before_prompt() {
        let args = base_tui_args(
            Some("session-123"),
            "resume prompt",
            Some("gpt-5-codex"),
            Some("high"),
            false,
            Some(true),
            None,
        );

        assert_eq!(args.first().map(String::as_str), Some("resume"));
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "-m" && pair[1] == "gpt-5-codex")
        );
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(args[separator - 1], "session-123");
        assert_eq!(args[separator + 1], "resume prompt");
    }

    #[test]
    fn codex_tui_config_overrides_are_inserted_before_prompt_delimiter() {
        let mut args = base_tui_args(
            None,
            "hello from tui",
            Some("gpt-5-codex"),
            None,
            false,
            None,
            None,
        );

        append_codex_config_overrides(&mut args, vec!["features.hooks=true".to_string()]);

        let override_index = args
            .windows(2)
            .position(|pair| pair[0] == "-c" && pair[1] == "features.hooks=true")
            .expect("expected hook feature override");
        let delimiter_index = args
            .iter()
            .position(|arg| arg == "--")
            .expect("expected prompt delimiter");
        assert!(override_index < delimiter_index);
    }

    #[test]
    fn codex_tui_config_overrides_preserve_resume_session_position() {
        let mut args = base_tui_args(
            Some("session-123"),
            "resume prompt",
            Some("gpt-5-codex"),
            None,
            false,
            None,
            None,
        );

        append_codex_config_overrides(&mut args, vec!["features.hooks=true".to_string()]);

        let delimiter_index = args
            .iter()
            .position(|arg| arg == "--")
            .expect("expected prompt delimiter");
        assert_eq!(args[delimiter_index - 1], "session-123");
        assert!(
            args[..delimiter_index - 1]
                .windows(2)
                .any(|pair| pair[0] == "-c" && pair[1] == "features.hooks=true")
        );
    }

    #[test]
    fn codex_legacy_tmux_script_preserves_wrapper_launch() {
        let script = render_codex_wrapper_tmux_script(
            "unset CLAUDECODE\n",
            "/tmp/agentdesk",
            "/tmp/out.jsonl",
            "/tmp/in.fifo",
            "/tmp/prompt.txt",
            "/work/repo",
            "/opt/bin/codex",
            Some("gpt-5-codex"),
            Some("thread-123"),
            Some(120_000),
            Some(false),
            Some(true),
        );

        assert!(script.contains("exec '/tmp/agentdesk' codex-tmux-wrapper"));
        assert!(script.contains("'--codex-bin'"));
        assert!(script.contains("'/opt/bin/codex'"));
        assert!(script.contains("'--codex-model'"));
        assert!(script.contains("'gpt-5-codex'"));
        assert!(script.contains("'--resume-session-id'"));
        assert!(script.contains("'thread-123'"));
        assert!(script.contains("'--compact-token-limit'"));
        assert!(script.contains("'120000'"));
        assert!(!script.contains("exec '/opt/bin/codex' "));
    }

    #[test]
    fn codex_runtime_kind_records_exec_json_policy() {
        assert!(!CodexRuntimeKind::DirectTui.uses_codex_exec_json());
        assert!(CodexRuntimeKind::LegacyWrapperFallback.uses_codex_exec_json());
        assert!(CodexRuntimeKind::ProcessBackend.uses_codex_exec_json());
        assert!(!CodexRuntimeKind::RemoteDirect.uses_codex_exec_json());
        assert!(!CodexRuntimeKind::RemoteTmux.uses_codex_exec_json());
        assert!(CodexRuntimeKind::DirectHeadless.uses_codex_exec_json());
        assert!(CodexRuntimeKind::SimpleHeadless.uses_codex_exec_json());
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use std::sync::mpsc;

    use super::{
        CODEX_BACKGROUND_TASK_NOTIFICATION_ID, CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS,
        TMUX_PROMPT_B64_PREFIX, base_exec_args, build_tmux_launch_env_lines, compose_codex_prompt,
        handle_codex_json_line, should_reuse_existing_provider_session,
    };
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::discord::restart_report::{
        RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
    };
    use crate::services::provider::ProviderKind;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
    use serde_json::Value;

    #[test]
    fn test_tmux_launch_env_lines_include_exec_path_and_report_envs() {
        let env_lines = build_tmux_launch_env_lines(
            Some("/tmp/provider:/usr/bin"),
            Some(42),
            Some(ProviderKind::Codex),
        );

        assert!(env_lines.contains("unset CLAUDECODE"));
        assert!(env_lines.contains("export PATH='/tmp/provider:/usr/bin'"));
        assert!(env_lines.contains(&format!("export {}=42", RESTART_REPORT_CHANNEL_ENV)));
        assert!(env_lines.contains(&format!("export {}=codex", RESTART_REPORT_PROVIDER_ENV)));
    }

    #[test]
    fn test_handle_codex_json_line_maps_thread_and_turn_completion() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"thread.started","thread_id":"thread-1"}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();
        let _ = handle_codex_json_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello"}} "#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();
        let done = handle_codex_json_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":3}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        assert_eq!(thread_id.as_deref(), Some("thread-1"));
        assert_eq!(done, Some(true));

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert!(matches!(items[0], StreamMessage::Init { .. }));
        assert!(matches!(items[1], StreamMessage::Text { .. }));
        assert!(matches!(items[2], StreamMessage::StatusUpdate { .. }));
        assert!(matches!(items[3], StreamMessage::Done { .. }));
    }

    #[test]
    fn test_handle_codex_json_line_maps_background_event_to_task_notification() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let done = handle_codex_json_line(
            r#"{"type":"background_event","message":"CI green"}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        assert_eq!(done, Some(false));

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 1);
        match &items[0] {
            StreamMessage::TaskNotification {
                task_id,
                status,
                summary,
                kind,
            } => {
                assert_eq!(task_id, CODEX_BACKGROUND_TASK_NOTIFICATION_ID);
                assert_eq!(status, CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS);
                assert_eq!(summary, "CI green");
                assert_eq!(
                    *kind,
                    crate::services::agent_protocol::TaskNotificationKind::Background
                );
            }
            other => panic!("Expected TaskNotification, got {:?}", other),
        }
    }

    #[test]
    fn test_compose_codex_prompt_includes_authoritative_sections() {
        let prompt = compose_codex_prompt(
            "role과 mission만 답해줘.",
            Some("role: PMD\nmission: 백로그 관리"),
            Some(&["Bash".to_string(), "Read".to_string()]),
        );

        assert!(prompt.contains("[Authoritative Instructions]"));
        assert!(prompt.contains("role: PMD"));
        assert!(!prompt.contains("[Tool Policy]"));
        assert!(!prompt.contains("Bash, Read"));
        assert!(prompt.contains("[User Request]\nrole과 mission만 답해줘."));
    }

    #[test]
    fn test_compose_codex_prompt_returns_plain_prompt_without_overrides() {
        let prompt = compose_codex_prompt("just answer", None, None);
        assert_eq!(prompt, "just answer");
    }

    #[test]
    fn test_provider_session_reuse_decision_honors_explicit_fresh_flag() {
        assert!(should_reuse_existing_provider_session(true, false));
        assert!(!should_reuse_existing_provider_session(true, true));
        assert!(!should_reuse_existing_provider_session(false, false));
        assert!(!should_reuse_existing_provider_session(false, true));
    }

    #[test]
    fn test_codex_reasoning_started_sends_thinking() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"item.started","item":{"type":"reasoning","id":"rs_001","summary":[]}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(
            items[0],
            StreamMessage::Thinking { summary: None }
        ));
    }

    #[test]
    fn test_codex_reasoning_completed_sends_redacted_thinking() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"item.completed","item":{"type":"reasoning","id":"rs_001","summary":[{"type":"summary_text","text":"Analyzing the code structure"}]}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(
            items[0],
            StreamMessage::Thinking { summary: None }
        ));
    }

    #[test]
    fn test_codex_mcp_tool_events_map_to_tool_use_and_result() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"mcp_tool_call_begin","call_id":"call-1","invocation":{"server":"memento","tool":"context","arguments":{"query":"foo","sessionId":"session-1"}}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();
        let _ = handle_codex_json_line(
            r#"{"type":"mcp_tool_call_end","call_id":"call-1","result":{"Ok":{"structuredContent":{"_searchEventId":"search-1","fragments":[{"id":"frag-1"}]}}}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 2);
        match &items[0] {
            StreamMessage::ToolUse { name, input } => {
                assert_eq!(name, "mcp__memento__context");
                assert_eq!(
                    serde_json::from_str::<Value>(input).unwrap(),
                    serde_json::json!({
                        "query": "foo",
                        "sessionId": "session-1",
                    })
                );
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
        match &items[1] {
            StreamMessage::ToolResult { content, is_error } => {
                assert!(!is_error);
                assert_eq!(
                    serde_json::from_str::<Value>(content).unwrap(),
                    serde_json::json!({
                        "_searchEventId": "search-1",
                        "fragments": [{"id": "frag-1"}],
                    })
                );
            }
            other => panic!("Expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn test_codex_mcp_tool_end_uses_text_payload_and_error_flag() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"mcp_tool_call_end","call_id":"call-1","result":{"Ok":{"content":[{"type":"text","text":"{\"success\":false,\"message\":\"boom\"}"}],"isError":true}}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 1);
        match &items[0] {
            StreamMessage::ToolResult { content, is_error } => {
                assert!(*is_error);
                assert_eq!(
                    serde_json::from_str::<Value>(content).unwrap(),
                    serde_json::json!({
                        "success": false,
                        "message": "boom",
                    })
                );
            }
            other => panic!("Expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn test_codex_mcp_tool_name_preserves_double_underscore_segments() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"mcp_tool_call_begin","call_id":"call-2","invocation":{"server":"memento__beta","tool":"context","arguments":{}}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 1);
        match &items[0] {
            StreamMessage::ToolUse { name, .. } => {
                assert_eq!(name, "mcp__memento__beta__context");
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_tmux_followup_encoding_is_single_line() {
        let prompt = "line1\nline2\nline3";
        let encoded = format!(
            "{}{}",
            TMUX_PROMPT_B64_PREFIX,
            BASE64_STANDARD.encode(prompt.as_bytes())
        );

        assert!(!encoded.contains('\n'));
    }

    #[test]
    fn test_base_exec_args_includes_model_before_exec() {
        let args = base_exec_args(
            None,
            "- starts like option",
            Some("gpt-5-codex"),
            Some("low"),
            false,
            Some(true),
            None,
        );
        assert!(args.starts_with(&[
            "-c".to_string(),
            r#"model_reasoning_effort="low""#.to_string(),
            "-m".to_string(),
            "gpt-5-codex".to_string(),
            "--enable".to_string(),
            "fast_mode".to_string(),
        ]));
        assert!(args.iter().any(|arg| arg == "exec"));
        let separator_index = args
            .iter()
            .position(|arg| arg == "--")
            .expect("prompt separator should be present");
        assert_eq!(
            args.get(separator_index + 1).map(String::as_str),
            Some("- starts like option")
        );
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "--skip-git-repo-check")
                .count(),
            1
        );
    }

    #[test]
    fn test_base_exec_args_includes_resume_before_flags() {
        let args = base_exec_args(
            Some("thread-123"),
            "hello",
            None,
            None,
            false,
            Some(false),
            None,
        );
        assert_eq!(
            args,
            vec![
                "--disable".to_string(),
                "fast_mode".to_string(),
                "exec".to_string(),
                "resume".to_string(),
                "thread-123".to_string(),
                "--skip-git-repo-check".to_string(),
                "--json".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "--".to_string(),
                "hello".to_string(),
            ]
        );
    }

    #[test]
    fn test_base_exec_args_uses_readonly_sandbox_when_requested() {
        let args = base_exec_args(None, "readonly", None, None, true, Some(false), None);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "read-only"])
        );
        assert!(args.starts_with(&["--disable".to_string(), "fast_mode".to_string()]));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox")
        );
    }

    #[test]
    fn test_base_exec_args_leaves_fast_mode_unset_when_not_overridden() {
        let args = base_exec_args(None, "hello", None, None, false, None, None);
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--enable" || arg == "--disable")
        );
        assert!(!args.iter().any(|arg| arg == "fast_mode"));
        assert!(args.starts_with(&["exec".to_string()]));
    }

    #[test]
    fn test_base_exec_args_includes_goals_feature_override() {
        let args = base_exec_args(None, "hello", None, None, false, None, Some(true));
        assert!(args.starts_with(&[
            "--enable".to_string(),
            "goals".to_string(),
            "exec".to_string()
        ]));
    }

    // ========== FollowupResult tests ==========

    #[cfg(unix)]
    #[test]
    fn test_codex_followup_fifo_not_found_returns_recreate() {
        use super::send_followup_to_tmux;
        use crate::services::provider::FollowupResult;

        let (sender, _receiver) = mpsc::channel();
        let dir = std::env::temp_dir();
        let output_path = dir.join(format!(
            "agentdesk-test-codex-followup-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::write(&output_path, "");

        let result = send_followup_to_tmux(
            "test prompt",
            output_path.to_str().unwrap(),
            "/tmp/agentdesk-test-codex-nonexistent-fifo",
            sender,
            None,
            "test-codex-followup",
        );

        let _ = std::fs::remove_file(&output_path);

        match result {
            Ok(FollowupResult::RecreateSession { error }) => {
                assert!(error.contains("Failed to open input FIFO"));
            }
            other => panic!("Expected Ok(RecreateSession), got {:?}", other),
        }
    }

    /// Regression test for #2249: on timeout, the spawned Codex child
    /// AND its grandchildren must be killed via SIGTERM→SIGKILL within
    /// the grace window, not left running as orphans. The grandchild
    /// assertion specifically exercises the `configure_child_process_group`
    /// path — without it, `kill_pid_tree` cannot reach descendants.
    #[cfg(unix)]
    #[test]
    fn execute_command_simple_with_timeout_kills_child_on_timeout() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let _env_guard = crate::services::discord::runtime_store::lock_test_env();

        let temp = tempfile::tempdir().expect("tempdir");
        let fake_codex = temp.path().join("fake-codex");
        let pid_file = temp.path().join("fake-codex.pid");
        let grandchild_pid_file = temp.path().join("fake-codex-grandchild.pid");

        // Fake codex: spawn a long-lived grandchild (its own subshell
        // sleep loop), record both PIDs, then loop forever. The
        // grandchild inherits the codex process group, so a SIGTERM
        // to the negative PID must reach it. If the parent spawn is
        // not in its own group, kill_pid_tree(-codex_pid) hits our
        // test runner's group instead and the grandchild survives —
        // which is exactly the bug #2249 keeps reintroducing.
        fs::write(
            &fake_codex,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$AGENTDESK_TEST_PID_FILE\"\n( sleep 600 & printf '%s' \"$!\" > \"$AGENTDESK_TEST_GRANDCHILD_PID_FILE\"; wait ) &\nwhile :; do sleep 1; done\n",
        )
        .expect("write fake codex");
        let mut perms = fs::metadata(&fake_codex).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_codex, perms).expect("set perms");

        let previous_codex_path = std::env::var_os("AGENTDESK_CODEX_PATH");
        let previous_pid_file = std::env::var_os("AGENTDESK_TEST_PID_FILE");
        let previous_grandchild_pid_file = std::env::var_os("AGENTDESK_TEST_GRANDCHILD_PID_FILE");

        // SAFETY: env mutations are serialized by lock_test_env().
        unsafe {
            std::env::set_var("AGENTDESK_CODEX_PATH", &fake_codex);
            std::env::set_var("AGENTDESK_TEST_PID_FILE", &pid_file);
            std::env::set_var("AGENTDESK_TEST_GRANDCHILD_PID_FILE", &grandchild_pid_file);
        }

        let started = Instant::now();
        let result = super::execute_command_simple_with_timeout(
            "ignored prompt",
            Duration::from_secs(1),
            "codex 2249 regression",
        );
        let elapsed = started.elapsed();

        // Restore env before any assert can panic out of this block.
        unsafe {
            match previous_codex_path {
                Some(value) => std::env::set_var("AGENTDESK_CODEX_PATH", value),
                None => std::env::remove_var("AGENTDESK_CODEX_PATH"),
            }
            match previous_pid_file {
                Some(value) => std::env::set_var("AGENTDESK_TEST_PID_FILE", value),
                None => std::env::remove_var("AGENTDESK_TEST_PID_FILE"),
            }
            match previous_grandchild_pid_file {
                Some(value) => std::env::set_var("AGENTDESK_TEST_GRANDCHILD_PID_FILE", value),
                None => std::env::remove_var("AGENTDESK_TEST_GRANDCHILD_PID_FILE"),
            }
        }

        let err = result.expect_err("expected timeout error");
        assert!(
            err.contains("codex 2249 regression timeout"),
            "unexpected error: {err}"
        );
        // Grace window is ~200ms in kill_pid_tree; allow generous
        // slack for CI scheduling but bound it well under a real
        // child's wall-clock lifetime so we know the kill fired.
        assert!(
            elapsed < Duration::from_secs(6),
            "timeout path took too long: {:?}",
            elapsed
        );

        // Confirm both the codex child and its grandchild are gone
        // within the SIGTERM (200ms grace) + SIGKILL window. If a PID
        // file never appeared, the shell never reached its printf —
        // we skip that specific assertion rather than treating a slow
        // CI scheduler as a regression.
        fn assert_pid_dead_within(pid_file: &std::path::Path, label: &str) {
            let pid_deadline = Instant::now() + Duration::from_secs(2);
            while !pid_file.exists() && Instant::now() < pid_deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !pid_file.exists() {
                return;
            }
            let pid_str = std::fs::read_to_string(pid_file).expect("pid file");
            let pid_str = pid_str.trim();
            if pid_str.is_empty() {
                return;
            }
            let kill_deadline = Instant::now() + Duration::from_secs(5);
            let mut still_alive = true;
            while Instant::now() < kill_deadline {
                let alive = std::process::Command::new("kill")
                    .args(["-0", pid_str])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !alive {
                    still_alive = false;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(
                !still_alive,
                "{label} pid {pid_str} survived past SIGTERM+SIGKILL grace window"
            );
        }

        assert_pid_dead_within(&pid_file, "codex child");
        assert_pid_dead_within(&grandchild_pid_file, "codex grandchild");
    }
}

#[cfg(test)]
mod remote_dispatch_gate_tests {
    //! Issue #2193 — regression tests for the Codex remote SSH dispatch
    //! gate inside `execute_command_streaming`. These tests verify that:
    //!
    //!   1. A remote profile + tmux + `AGENTDESK_CODEX_REMOTE_TMUX=true`
    //!      is hard-refused as a policy non-goal, regardless of the
    //!      direct-SSH gate state.
    //!   2. A remote profile without the env var is refused when the
    //!      runtime gate is off (the default).
    //!
    //! Both refusals MUST happen before any SSH attempt, so they return
    //! `Err` synchronously and never call into `services::remote_stub`.

    use crate::services::remote::{RemoteAuth, RemoteProfile};
    use std::sync::Mutex;

    static GATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fixture_profile() -> RemoteProfile {
        RemoteProfile {
            name: "test-mac-mini".to_string(),
            host: "mac-mini.local".to_string(),
            port: 22,
            user: "operator".to_string(),
            auth: RemoteAuth::KeyFile {
                path: "/dev/null".to_string(),
                passphrase: None,
            },
            default_path: "/tmp".to_string(),
            claude_path: None,
        }
    }

    fn run_dispatch(
        tmux_session_name: Option<&str>,
        remote_tmux_env: Option<&str>,
    ) -> Result<(), String> {
        // Lock so the AGENTDESK_CODEX_REMOTE_TMUX env mutation and the
        // provider_hosting runtime cell can't race with other tests.
        let _guard = GATE_TEST_LOCK.lock().unwrap();

        // Default gate: OFF. This mirrors fresh-bootstrap state.
        crate::services::provider_hosting::install_provider_hosting_config(
            &crate::config::Config::default(),
        );

        // Manipulate the env var the dispatch path reads.
        match remote_tmux_env {
            Some(v) => unsafe { std::env::set_var("AGENTDESK_CODEX_REMOTE_TMUX", v) },
            None => unsafe { std::env::remove_var("AGENTDESK_CODEX_REMOTE_TMUX") },
        }

        let profile = fixture_profile();
        let (tx, _rx) = std::sync::mpsc::channel();
        let result = super::execute_command_streaming(
            "ignored prompt",
            None,
            "/tmp",
            tx,
            None,
            None,
            None,
            Some(&profile),
            tmux_session_name,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
        );

        // Clean up the env var so we don't leak state.
        unsafe { std::env::remove_var("AGENTDESK_CODEX_REMOTE_TMUX") };

        result
    }

    /// Remote tmux is a policy non-goal and MUST be refused even if the
    /// direct-SSH gate is later opened — i.e. the refusal does not
    /// depend on `remote_ssh_enabled` or `PREREQUISITES_SATISFIED`.
    #[cfg(unix)]
    #[test]
    fn remote_tmux_is_hard_refused_as_policy_non_goal() {
        let err = run_dispatch(Some("test-session"), Some("true"))
            .expect_err("remote tmux must be refused");
        assert!(
            err.contains("non-goal"),
            "expected non-goal refusal, got: {err}"
        );
        assert!(err.contains("#2193"), "refusal must cite the issue: {err}");
    }

    /// Direct remote SSH dispatch (no tmux env) is refused while the
    /// gate is off. This guards against a future change wiring real
    /// `RemoteProfile` lists before flipping `remote_ssh_enabled`.
    #[test]
    fn remote_direct_is_refused_when_gate_off() {
        let err = run_dispatch(None, None).expect_err("remote SSH must be refused while gate off");
        assert!(
            err.contains("disabled by policy"),
            "expected policy refusal, got: {err}"
        );
        assert!(err.contains("#2193"), "refusal must cite the issue: {err}");
    }
}
