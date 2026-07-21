use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
#[cfg(unix)]
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::services::agent_protocol::{RuntimeHandoff, StreamMessage, is_valid_session_id};
use crate::services::claude;
use crate::services::claude_tui::hook_bundle::{
    HookBundleConfig, codex_hook_config_overrides, run_codex_hook_launch_self_check_with_exec_path,
};
use crate::services::claude_tui::hook_server::current_hook_endpoint;
use crate::services::discord::restart_report::{
    RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
};
use crate::services::process::{kill_child_tree, shell_escape};
use crate::services::provider::{
    CancelToken, FollowupResult, ProviderKind, SessionProbe, cancel_requested,
    fold_read_output_result, is_readonly_tool_policy, register_child_pid, spawn_cancel_watchdog,
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
const TMUX_PROMPT_B64_CHUNK_PREFIX: &str = "__AGENTDESK_B64_CHUNK__:";
const TMUX_PROMPT_B64_CHUNK_SIZE: usize = 700;
pub(crate) const CODEX_BACKGROUND_TASK_NOTIFICATION_ID: &str = "codex-background-event";
pub(crate) const CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS: &str = "completed";

#[cfg(unix)]
type CodexTuiSessionTurnLock = Arc<Mutex<()>>;

#[cfg(unix)]
static CODEX_TUI_SESSION_TURN_LOCKS: LazyLock<dashmap::DashMap<String, CodexTuiSessionTurnLock>> =
    LazyLock::new(dashmap::DashMap::new);

#[cfg(unix)]
fn codex_tui_session_turn_lock(tmux_session_name: &str) -> CodexTuiSessionTurnLock {
    CODEX_TUI_SESSION_TURN_LOCKS
        .entry(tmux_session_name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexRuntimeKind {
    DirectTui,
    LegacyWrapperFallback,
    ProcessBackend,
    RemoteDirect,
    // #3034: runtime-kind taxonomy variant; constructed only in tests today.
    #[allow(dead_code)]
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
    pub(crate) developer_instructions: Option<String>,
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
        self.resume_session_id =
            clean_nonempty(value).filter(|session_id| is_valid_session_id(session_id));
        self
    }

    pub(crate) fn with_developer_instructions(mut self, value: Option<&str>) -> Self {
        self.developer_instructions = clean_nonempty(value);
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
    let probe = crate::services::platform::probe_provider_binary_version("codex");
    log_codex_binary_probe(&probe);
    probe.resolution
}

fn log_codex_binary_probe(probe: &crate::services::platform::binary_resolver::BinaryVersionProbe) {
    let resolution = &probe.resolution;
    let candidate_diagnostics = resolution
        .attempts
        .iter()
        .filter(|attempt| {
            attempt.starts_with("selected_candidate_version:")
                || attempt.starts_with("skipped_candidate_success:")
                || attempt.starts_with("skipped_candidate_failure:")
        })
        .cloned()
        .collect::<Vec<_>>()
        .join(" | ");
    tracing::info!(
        provider = "codex",
        codex_bin = resolution.resolved_path.as_deref().unwrap_or("unknown"),
        codex_cli_version = probe.version_output.as_deref().unwrap_or("unknown"),
        source = resolution.source.as_deref().unwrap_or("unknown"),
        probe_failure_kind = probe.probe_failure_kind.as_deref().unwrap_or("none"),
        candidate_diagnostics = candidate_diagnostics.as_str(),
        "codex launch binary resolved"
    );
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

#[allow(clippy::too_many_arguments)]
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
    developer_instructions: Option<&str>,
    compact_token_limit: Option<u64>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
) -> String {
    let reasoning_effort = codex_reasoning_effort_from_env();
    let wrapper_args = build_codex_wrapper_cli_args(
        &CodexLaunchOptions::new("")
            .with_resume_session_id(session_id)
            .with_developer_instructions(developer_instructions)
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
        --cwd {wd} \\\n  \
        --input-mode fifo{wrapper_args}\n",
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

fn insert_codex_resume_option_before_other_options(args: &mut Vec<String>, option: &str) {
    args.insert(0, option.to_string());
}

fn codex_resume_help_mentions_hook_trust_bypass(help_text: &str) -> bool {
    help_text.contains("--dangerously-bypass-hook-trust")
}

fn codex_resume_supports_hook_trust_bypass(
    codex_bin: &str,
    resolution: &crate::services::platform::BinaryResolution,
) -> bool {
    let mut command = Command::new(codex_bin);
    crate::services::platform::apply_binary_resolution(&mut command, resolution);
    command
        .args(["resume", "--help"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match command.spawn() {
        Ok(mut child) => {
            let deadline = Instant::now() + Duration::from_secs(2);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            tracing::warn!(
                                codex_bin,
                                "timed out inspecting Codex resume help for hook trust bypass support"
                            );
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        tracing::warn!(
                            codex_bin,
                            error = %error,
                            "could not wait for Codex resume help probe"
                        );
                        return false;
                    }
                }
            };
            let mut help_text = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_string(&mut help_text);
            }
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut help_text);
            }
            status.success() && codex_resume_help_mentions_hook_trust_bypass(&help_text)
        }
        Err(error) => {
            tracing::warn!(
                codex_bin,
                error = %error,
                "could not inspect Codex resume help for hook trust bypass support"
            );
            false
        }
    }
}

pub(crate) fn codex_direct_tui_hook_overrides_enabled() -> bool {
    std::env::var("AGENTDESK_CODEX_DIRECT_TUI_HOOKS")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

fn codex_config_overrides(options: &CodexLaunchOptions) -> Vec<String> {
    let mut overrides = Vec::new();
    if let Some(effort) = options.reasoning_effort.as_deref() {
        overrides.push(format!("model_reasoning_effort={effort:?}"));
    }
    if let Some(limit) = options.compact_token_limit.filter(|limit| *limit > 0) {
        overrides.push(format!("model_auto_compact_token_limit={limit}"));
    }
    if let Some(instructions) = options.developer_instructions.as_deref() {
        overrides.push(format!("developer_instructions={instructions:?}"));
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

// #3034: exercised only by the codex arg-builder unit tests below.
#[allow(dead_code)]
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
    if let Some(instructions) = options.developer_instructions.as_deref() {
        args.push("--developer-instructions".to_string());
        args.push(instructions.to_string());
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

fn register_codex_tui_idle_relay_binding(
    tmux_session_name: &str,
    tail_result: &crate::services::codex_tui::rollout_tail::CodexTuiTailResult,
) {
    if let Err(error) = crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        tmux_session_name,
        &tail_result.rollout_path,
        tail_result.session_id.as_deref(),
    ) {
        tracing::warn!(
            tmux_session_name,
            rollout_path = %tail_result.rollout_path.display(),
            error,
            "failed to persist Codex TUI rollout marker; restart rehydrate will fall back to rollout discovery"
        );
    }
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux_session_name,
        codex_tui_idle_relay_binding(tmux_session_name, tail_result),
    );
}

fn codex_tui_idle_relay_binding(
    tmux_session_name: &str,
    tail_result: &crate::services::codex_tui::rollout_tail::CodexTuiTailResult,
) -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
    let relay_output_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let relay_last_offset = std::fs::metadata(&relay_output_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        output_path: tail_result.rollout_path.display().to_string(),
        relay_output_path: Some(relay_output_path),
        input_fifo_path: None,
        session_id: tail_result.session_id.clone(),
        last_offset: tail_result.final_offset,
        relay_last_offset: Some(relay_last_offset),
    }
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
    crate::services::tui_prompt_dedupe::register_provider_session(
        ProviderKind::Codex.as_str(),
        hook_session_id,
        tmux_session_name,
    );
    codex_hook_config_overrides(&config)
}

fn should_reuse_existing_provider_session(
    existing_session_usable: bool,
    force_fresh_provider_session: bool,
) -> bool {
    existing_session_usable && !force_fresh_provider_session
}

fn should_preserve_live_reused_provider_session(
    resume_session_id: Option<&str>,
    has_live_pane: bool,
    force_fresh_provider_session: bool,
) -> bool {
    resume_session_id
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
        && has_live_pane
        && !force_fresh_provider_session
}

/// Input transport a live Codex tmux wrapper was launched with, inferred from
/// its launch `.sh` marker. `Unknown` covers a missing/unreadable marker (e.g.
/// a dcserver restart that lost /tmp), which is distinct from an explicit
/// legacy `Pipe` marker — the two must be treated differently when deciding
/// whether to preserve a live pane.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexWrapperInputMode {
    Fifo,
    Pipe,
    Unknown,
}

#[cfg(unix)]
fn codex_wrapper_script_input_mode(script: &str) -> CodexWrapperInputMode {
    let mentions = |needle: &str| {
        script
            .lines()
            .any(|line| line.trim() == format!("{needle} \\") || line.trim() == needle)
    };
    if mentions("--input-mode fifo") {
        CodexWrapperInputMode::Fifo
    } else if mentions("--input-mode pipe") {
        CodexWrapperInputMode::Pipe
    } else {
        CodexWrapperInputMode::Unknown
    }
}

#[cfg(unix)]
fn codex_tmux_wrapper_session_input_mode(tmux_session_name: &str) -> CodexWrapperInputMode {
    let Some(script_path) =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "sh")
    else {
        return CodexWrapperInputMode::Unknown;
    };
    std::fs::read_to_string(script_path)
        .map(|script| codex_wrapper_script_input_mode(&script))
        .unwrap_or(CodexWrapperInputMode::Unknown)
}

#[cfg(unix)]
fn codex_fifo_wrapper_session_usable(
    has_live_pane: bool,
    has_output_path: bool,
    input_mode: CodexWrapperInputMode,
    has_input_fifo_path: bool,
) -> bool {
    // A reused FIFO wrapper needs the live pane / output transport, an explicit
    // FIFO input mode this build can drive, and a resolvable input FIFO, because
    // the follow-up path now writes the base64 sentinel line directly into that
    // FIFO (issue #3001). Without the FIFO file there is nothing to write to, so
    // the session is not reusable.
    has_live_pane
        && has_output_path
        && input_mode == CodexWrapperInputMode::Fifo
        && has_input_fifo_path
}

/// Whether a live, resume-eligible Codex pane should be preserved (not killed)
/// when its I/O files are unavailable. Preserve FIFO-mode panes this build can
/// drive and Unknown panes (missing marker after restart), but NOT an explicit
/// legacy pipe-mode pane — that one cannot be driven here and must be recreated
/// (which resumes the conversation via session id) instead of stranding.
#[cfg(unix)]
fn codex_input_mode_allows_live_session_preservation(input_mode: CodexWrapperInputMode) -> bool {
    !matches!(input_mode, CodexWrapperInputMode::Pipe)
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexTmuxTerminationReason {
    reason_code: &'static str,
    reason_text: &'static str,
}

#[cfg(unix)]
fn codex_tui_existing_session_termination_reason(
    force_fresh_provider_session: bool,
    has_live_pane: bool,
) -> CodexTmuxTerminationReason {
    if force_fresh_provider_session {
        CodexTmuxTerminationReason {
            reason_code: "fresh_provider_session_requested",
            reason_text: "codex tui fresh provider session requested",
        }
    } else if has_live_pane {
        CodexTmuxTerminationReason {
            reason_code: "session_restart_before_direct_launch",
            reason_text: "codex tui local session restart before direct launch",
        }
    } else {
        CodexTmuxTerminationReason {
            reason_code: "stale_session_recreate",
            reason_text: "stale codex tui local session cleanup before recreate",
        }
    }
}

#[cfg(unix)]
fn codex_wrapper_existing_session_termination_reason(
    force_fresh_provider_session: bool,
) -> CodexTmuxTerminationReason {
    if force_fresh_provider_session {
        CodexTmuxTerminationReason {
            reason_code: "fresh_provider_session_requested",
            reason_text: "codex fresh provider session requested",
        }
    } else {
        CodexTmuxTerminationReason {
            reason_code: "stale_session_recreate",
            reason_text: "stale local session cleanup before recreate",
        }
    }
}

#[cfg(unix)]
fn record_codex_tmux_termination(
    tmux_session_name: &str,
    killer_component: &str,
    reason_code: &str,
    reason_text: &str,
    last_offset: Option<u64>,
) {
    crate::services::termination_audit::record_termination_for_tmux(
        tmux_session_name,
        None,
        killer_component,
        reason_code,
        Some(reason_text),
        last_offset,
    );
}

#[cfg(unix)]
use crate::services::tmux_common::{tmux_owner_path, write_tmux_owner_marker};

/// Cancel-aware variant of the model-override simple execution path.
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
// #3034: retained for the #2387 timeout-drain regression test below.
#[allow(dead_code)]
fn execute_command_simple_with_timeout_worker<F>(
    timeout: Duration,
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
        // Clear the registered child PID *before* sending the result.
        // The Child has already been reaped by wait_with_output() inside
        // execute_command_simple_cancellable, so the kernel may recycle
        // this PID at any moment. Clearing here prevents a late timeout
        // path (timer raced ahead of recv on a different timeline) from
        // signalling a reused, unrelated PID.
        cancel_for_worker.clear_child_pid();
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
            // request_cleanup owns signal delivery and leaves the PID visible
            // until the worker clears it, preserving the timeout drain meaning.
            let child_pid = cancel_token.child_pid_value();
            let child_pid_was_none = child_pid.is_none();
            if let Some(pid) = child_pid {
                tracing::warn!(
                    provider = provider_name,
                    stage = %label_owned,
                    child_pid = pid,
                    "execute_command_simple_with_timeout cleanup signal dispatched for child process group"
                );
            } else {
                tracing::warn!(
                    provider = provider_name,
                    stage = %label_owned,
                    "execute_command_simple_with_timeout had no registered child PID at cancel time"
                );
            }
            // Bounded drain: wait up to 3s for the worker to observe the
            // kill, drop its sender, and let the channel close. Only
            // join() if we actually saw the worker hand back a result;
            // otherwise drop the JoinHandle and let the OS reap the
            // thread when this process exits, rather than blocking the
            // caller forever on a stuck wait_with_output.
            if let Ok(result) = rx.recv_timeout(Duration::from_secs(3)) {
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
            "codex 2387 regression",
            "codex",
            |_cancel_token| {
                std::thread::sleep(Duration::from_millis(40));
                Ok("codex natural completion".to_string())
            },
        );

        assert_eq!(result.unwrap(), "codex natural completion");
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
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            &ProviderKind::Codex,
            entrypoint_supports_tui_hosting,
            report_channel_id,
        );
    session_selection.log_start("codex.execute_command_streaming");

    let readonly_mode = is_readonly_tool_policy(allowed_tools);
    let developer_instructions = compose_codex_developer_instructions(system_prompt, allowed_tools);
    let prompt = compose_codex_prompt(prompt, None, allowed_tools);

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
                    developer_instructions.as_deref(),
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
                developer_instructions.as_deref(),
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
            developer_instructions.as_deref(),
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
        developer_instructions.as_deref(),
        readonly_mode,
        compact_token_limit,
    )
}

fn compose_codex_prompt(
    prompt: &str,
    _system_prompt: Option<&str>,
    _allowed_tools: Option<&[String]>,
) -> String {
    prompt.to_string()
}

pub(crate) fn compose_codex_developer_instructions(
    system_prompt: Option<&str>,
    _allowed_tools: Option<&[String]>,
) -> Option<String> {
    system_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|system_prompt| {
            format!(
                "{system_prompt}\n\nThese instructions are authoritative for this turn. Follow them over any generic assistant persona unless the user explicitly asks to inspect or compare them."
            )
        })
}

#[allow(clippy::too_many_arguments)]
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
    developer_instructions: Option<&str>,
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
            .with_developer_instructions(developer_instructions)
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
    crate::services::process::configure_child_process_group(&mut command);

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start Codex: {}", e))?;

    register_child_pid(cancel_token.as_deref(), child.id());
    let _cancel_watchdog = spawn_cancel_watchdog(cancel_token.clone(), "codex-direct-stream");
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

#[allow(clippy::too_many_arguments)]
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

/// Tear down a pre-existing tmux session before relaunching a Codex Direct TUI
/// on the same name. Records the termination audit + exit reason and kills the
/// session. No-op when the session does not exist.
#[cfg(unix)]
fn cleanup_existing_codex_tui_session(
    tmux_session_name: &str,
    force_fresh_provider_session: bool,
    has_live_pane: bool,
    warm_fallback_reason: Option<
        crate::services::codex_tui::warm_followup::CodexWarmFallbackReason,
    >,
    pane_already_stopped: bool,
) {
    let legacy_reason;
    let (reason_code, reason_text) = if let Some(reason) = warm_fallback_reason {
        (reason.reason_code(), reason.reason_text())
    } else {
        legacy_reason = codex_tui_existing_session_termination_reason(
            force_fresh_provider_session,
            has_live_pane,
        );
        (legacy_reason.reason_code, legacy_reason.reason_text)
    };
    record_codex_tmux_termination(
        tmux_session_name,
        "codex_tui_provider",
        reason_code,
        reason_text,
        None,
    );
    record_tmux_exit_reason(tmux_session_name, reason_text);
    if !pane_already_stopped {
        crate::services::platform::tmux::kill_session(tmux_session_name, reason_text);
    }
}

/// Paths and timing produced while preparing a Codex Direct TUI launch script.
#[cfg(unix)]
struct CodexTuiLaunchScript {
    script_path: String,
    owner_path: String,
    rollout_modified_since: std::time::SystemTime,
}

/// Resolve the Codex binary, build the launch args + env, render and write the
/// launch script, and register the Discord-originated prompt for dedupe.
///
/// Returns the resolved binary, script path, owner-marker path, and the
/// rollout "modified since" stamp captured just before the script is written.
/// Errors propagate exactly as the inline body did (`?`).
#[cfg(unix)]
fn prepare_codex_tui_launch_script(
    tmux_session_name: &str,
    session_id: Option<&str>,
    prompt: &str,
    launch_options: &CodexLaunchOptions,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    warm_followup_enabled: bool,
) -> Result<CodexTuiLaunchScript, String> {
    write_tmux_owner_marker(tmux_session_name)?;
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux_session_name,
        crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
    )?;
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
    let mut args = build_codex_tui_args(launch_options);
    let codex_hook_overrides = if codex_direct_tui_hook_overrides_enabled() {
        prepare_codex_tui_hook_overrides(
            tmux_session_name,
            session_id,
            &codex_bin,
            resolution.exec_path.as_deref(),
        )
    } else {
        tracing::info!(
            tmux_session_name,
            "Codex direct TUI session hook overrides disabled; using rollout transcript tail for relay"
        );
        Vec::new()
    };
    if !codex_hook_overrides.is_empty() {
        append_codex_config_overrides(&mut args, codex_hook_overrides);
        if codex_resume_supports_hook_trust_bypass(&codex_bin, &resolution) {
            insert_codex_resume_option_before_other_options(
                &mut args,
                "--dangerously-bypass-hook-trust",
            );
        } else {
            tracing::warn!(
                codex_bin,
                "Codex resume does not advertise --dangerously-bypass-hook-trust; relying on session hook trust hashes"
            );
        }
    }
    let script_content = render_codex_tui_tmux_script(&env_lines, &codex_bin, &args);
    let rollout_modified_since = std::time::SystemTime::now();

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write Codex TUI launch script: {}", e))?;
    if warm_followup_enabled {
        crate::services::codex_tui::session::write_codex_tui_launch_options_fingerprint(
            tmux_session_name,
            &crate::services::codex_tui::warm_followup::codex_tui_launch_options_fingerprint(
                launch_options,
            ),
        )?;
    }
    crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
        ProviderKind::Codex.as_str(),
        tmux_session_name,
        prompt,
    );
    if let Some(channel_id) = report_channel_id {
        crate::services::tui_prompt_dedupe::register_tmux_channel(tmux_session_name, channel_id);
    }

    Ok(CodexTuiLaunchScript {
        script_path,
        owner_path,
        rollout_modified_since,
    })
}

/// Wire the cancel token to the freshly created tmux session: record the
/// session name and (best-effort) the pane PID so a later /stop can target it.
#[cfg(unix)]
pub(crate) fn wire_cancel_token_to_tmux_session(
    cancel_token: Option<&std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) {
    if let Some(token) = cancel_token {
        token.bind_unmanaged_session_name(tmux_session_name);
        if let Some(pid) = crate::services::platform::tmux::pane_pid(tmux_session_name) {
            token.store_child_pid(pid);
        }
    }
}

/// Dispatch the rollout tail for the Direct TUI launch: a resume tails the
/// selected session's rollout from its committed offset, otherwise we tail the
/// latest rollout for the cwd. Mirrors the inline resume-vs-fresh branch.
///
/// `resume` carries the validated `(rollout_path, start_offset,
/// selected_session_id)` for the resume branch; `None` selects the fresh tail.
/// The resume-metadata validation lives in the orchestrator so a missing
/// rollout-path / session-id short-circuits the launch *before* any tail
/// dispatch (no tmux leak-kill), matching the pre-refactor inline ordering.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn dispatch_codex_tui_rollout_tail(
    resume: Option<(&std::path::Path, u64, &str)>,
    working_dir: &str,
    rollout_modified_since: std::time::SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    prompt: &str,
) -> Result<crate::services::codex_tui::rollout_tail::CodexTuiTailResult, String> {
    if let Some((rollout_path, start_offset, selected_session_id)) = resume {
        crate::services::codex_tui::rollout_tail::tail_resumed_rollout_for_session_with_handoff_for_tmux(
            std::path::Path::new(working_dir),
            selected_session_id,
            rollout_path,
            start_offset,
            rollout_modified_since,
            sender,
            cancel_token,
            || tmux_session_has_live_pane(tmux_session_name),
            tmux_session_name,
            Some(prompt),
        )
    } else {
        crate::services::codex_tui::rollout_tail::tail_latest_rollout_for_cwd_with_handoff_for_tmux(
            std::path::Path::new(working_dir),
            rollout_modified_since,
            sender,
            cancel_token,
            || tmux_session_has_live_pane(tmux_session_name),
            tmux_session_name,
            Some(prompt),
        )
    }
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
    developer_instructions: Option<&str>,
    readonly_mode: bool,
    compact_token_limit: Option<u64>,
    force_fresh_provider_session: bool,
) -> Result<(), String> {
    let warm_followup_enabled =
        crate::services::codex_tui::warm_followup::codex_tui_warm_followup_enabled();
    let turn_lock = warm_followup_enabled.then(|| codex_tui_session_turn_lock(tmux_session_name));
    let _turn_guard = turn_lock
        .as_ref()
        .map(|lock| lock.lock().unwrap_or_else(|error| error.into_inner()));
    let session_selection = crate::services::codex_tui::session::resolve_codex_tui_session(
        session_id,
        std::path::Path::new(working_dir),
        None,
        force_fresh_provider_session,
    );
    let reasoning_effort = codex_reasoning_effort_from_env();
    let launch_options = CodexLaunchOptions::new(prompt)
        .with_resume_session_id(session_selection.resume_session_id())
        .with_developer_instructions(developer_instructions)
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
            developer_instructions,
            compact_token_limit,
            force_fresh_provider_session,
        );
    }

    let session_exists = tmux_session_exists(tmux_session_name);
    let has_live_pane = tmux_session_has_live_pane(tmux_session_name);
    let mut warm_fallback_reason = None;
    let mut warm_fallback_pane_stopped = false;

    // Emergency kill switch: when disabled, bypass the warm module entirely
    // and retain the pre-#4411 cleanup/relaunch path below.
    if warm_followup_enabled {
        match crate::services::codex_tui::warm_followup::try_codex_tui_warm_followup(
            &session_selection,
            &launch_options,
            force_fresh_provider_session,
            session_exists,
            has_live_pane,
            prompt,
            sender.clone(),
            cancel_token.clone(),
            tmux_session_name,
            report_channel_id,
        ) {
            crate::services::codex_tui::warm_followup::CodexWarmFollowupOutcome::Terminal(
                result,
            ) => return result,
            crate::services::codex_tui::warm_followup::CodexWarmFollowupOutcome::Fallback(
                reason,
            ) => warm_fallback_reason = Some(reason),
            crate::services::codex_tui::warm_followup::CodexWarmFollowupOutcome::FallbackAfterPaneKill(
                reason,
            ) => {
                warm_fallback_reason = Some(reason);
                warm_fallback_pane_stopped = true;
            }
            crate::services::codex_tui::warm_followup::CodexWarmFollowupOutcome::LegacyPath => {}
        }
    }

    if session_exists {
        cleanup_existing_codex_tui_session(
            tmux_session_name,
            force_fresh_provider_session,
            has_live_pane,
            warm_fallback_reason,
            warm_fallback_pane_stopped,
        );
    }

    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);
    crate::services::codex_tui::session::CodexTuiSessionFiles::for_tmux_session(tmux_session_name)
        .cleanup_best_effort();

    let CodexTuiLaunchScript {
        script_path,
        owner_path,
        rollout_modified_since,
    } = prepare_codex_tui_launch_script(
        tmux_session_name,
        session_id,
        prompt,
        &launch_options,
        report_channel_id,
        report_provider,
        warm_followup_enabled,
    )?;

    let tmux_result = crate::services::platform::tmux::create_session(
        tmux_session_name,
        Some(working_dir),
        &format!("bash {}", shell_escape(&script_path)),
    )?;

    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        let _ = std::fs::remove_file(&owner_path);
        let _ = std::fs::remove_file(&script_path);
        crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
            ProviderKind::Codex.as_str(),
            tmux_session_name,
            prompt,
        );
        return Err(format!("tmux error: {}", stderr));
    }

    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    // #3087: stamp a per-spawn nonce on the Codex-TUI DIRECT spawn path too.
    // Without it this path produces no `.spawn_nonce`, so the status-panel
    // instance key is `None` and the new-session boundary cannot be detected.
    if let Err(e) = crate::services::discord::write_spawn_nonce(tmux_session_name) {
        tracing::warn!("failed to write spawn nonce for {tmux_session_name} (codex-tui): {e}");
    }

    wire_cancel_token_to_tmux_session(cancel_token.as_ref(), tmux_session_name);

    // #2172 cancel boundary: keep a clone of the cancel token so that
    // post-tail emission (RuntimeReady / SessionDied Done) and the
    // tail-error tmux-cleanup branch can BOTH consult `cancel_requested`
    // without re-acquiring it. The tail call itself moves its clone into
    // the rollout-tail thread; this clone stays in the launch frame.
    let cancel_token_for_post_tail = cancel_token.clone();
    // Resume-metadata validation runs HERE — after the cancel-token clone and
    // immediately before the tail dispatch, matching the pre-refactor inline
    // ordering. A missing rollout-path / session-id short-circuits the whole
    // launch with Err *without* the tail-error tmux leak-kill, exactly as the
    // original did (the validations were `?` above the tail_result assignment).
    let resume_params = if session_selection.resume {
        let rollout_path = session_selection
            .rollout_path
            .as_deref()
            .ok_or_else(|| "Codex TUI resume selected without rollout path".to_string())?;
        let start_offset = session_selection.rollout_start_offset.unwrap_or(0);
        let selected_session_id = session_selection
            .selected_session_id
            .as_deref()
            .ok_or_else(|| "Codex TUI resume selected without session id".to_string())?;
        Some((rollout_path, start_offset, selected_session_id))
    } else {
        None
    };
    let tail_result = dispatch_codex_tui_rollout_tail(
        resume_params,
        working_dir,
        rollout_modified_since,
        sender.clone(),
        cancel_token,
        tmux_session_name,
        prompt,
    );
    let tail_result = match resolve_codex_tui_tail_result(
        tail_result,
        cancel_token_for_post_tail.as_ref(),
        tmux_session_name,
    )? {
        Some(result) => result,
        // Cancel observed before the transcript was discovered: suppress the
        // tail Err and defer tmux cleanup to the cancel path (return Ok(())).
        None => return Ok(()),
    };

    emit_codex_tui_post_tail_handoff(
        tail_result,
        sender,
        cancel_token_for_post_tail,
        tmux_session_name,
    )
}

/// Resolve the rollout-tail result for the Codex Direct TUI launch.
///
/// - `Ok(Some(result))` — tail succeeded; proceed to post-tail emission.
/// - `Ok(None)` — tail failed but a cancel was observed first; the caller
///   must suppress the Err and return `Ok(())` (the bridge's cancel arm owns
///   finalisation and tmux teardown).
/// - `Err(error)` — genuine tail failure; the tmux session has been killed to
///   avoid a leak and the error propagates.
#[cfg(unix)]
fn resolve_codex_tui_tail_result(
    tail_result: Result<crate::services::codex_tui::rollout_tail::CodexTuiTailResult, String>,
    cancel_token_for_post_tail: Option<&std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<Option<crate::services::codex_tui::rollout_tail::CodexTuiTailResult>, String> {
    let cancel_observed =
        || crate::services::provider::cancel_requested(cancel_token_for_post_tail.map(|t| &**t));

    match tail_result {
        Ok(result) => Ok(Some(result)),
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
            // Return Ok(None) with no StreamMessage emitted: the producer
            // is silent post-cancel and the bridge's cancel arm drives
            // finalisation.
            if cancel_observed() {
                tracing::info!(
                    tmux_session = tmux_session_name,
                    error = %error,
                    "Codex rollout tail cancelled before transcript; suppressing tail Err and deferring tmux cleanup to cancel path"
                );
                return Ok(None);
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
            record_codex_tmux_termination(
                tmux_session_name,
                "codex_tui_provider",
                "rollout_tail_failed",
                &format!("codex rollout tail failed: {error}"),
                None,
            );
            record_tmux_exit_reason(tmux_session_name, &format!("rollout tail failed: {error}"));
            crate::services::platform::tmux::kill_session(
                tmux_session_name,
                "codex rollout tail failed",
            );
            Err(error)
        }
    }
}

/// Post-tail StreamMessage emission for the Codex Direct TUI launch: handles
/// the cancel-suppression guards, the SessionDied failure `Done`, the idle
/// relay binding, and the gated RuntimeReady handoff (with its readiness /
/// session-death / timeout outcomes). Always returns `Ok(())`; early returns
/// stand in for the orchestrator's post-cancel suppression paths.
#[cfg(unix)]
pub(crate) fn emit_codex_tui_post_tail_handoff(
    tail_result: crate::services::codex_tui::rollout_tail::CodexTuiTailResult,
    sender: Sender<StreamMessage>,
    cancel_token_for_post_tail: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<(), String> {
    let cancel_observed =
        || crate::services::provider::cancel_requested(cancel_token_for_post_tail.as_deref());

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
    if let crate::services::provider::ReadOutputResult::SessionDied { offset } = read_result {
        record_codex_tmux_termination(
            tmux_session_name,
            "codex_tui_provider",
            "session_died_before_response",
            "codex tui session ended before producing a response",
            Some(offset),
        );
        let _ = sender.send(StreamMessage::Done {
            result: "⚠ Codex TUI session ended before producing a response.".to_string(),
            session_id: None,
        });
    } else {
        // The Discord turn bridge only needs RuntimeReady when the TUI is
        // actually ready for another routed turn. The idle SSH-direct relay is
        // different: it scans Codex's rollout after the bridge has gone idle,
        // so it still needs the rollout binding even when RuntimeReady is
        // suppressed by the post-turn readiness guard.
        register_codex_tui_idle_relay_binding(tmux_session_name, &tail_result);

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
                record_codex_tmux_termination(
                    tmux_session_name,
                    "codex_tui_provider",
                    "session_died_before_input_ready",
                    "codex tui session ended before becoming input-ready",
                    Some(tail_result.final_offset),
                );
                let _ = sender.send(StreamMessage::Done {
                    result: "⚠ Codex TUI session ended before becoming input-ready.".to_string(),
                    session_id: tail_result.session_id.clone(),
                });
            }
            Err(error) => {
                // #2399 HIGH 2: composer did not redraw within the 200ms
                // probe budget. The previous behaviour emitted
                // `RuntimeReady` anyway "best-effort", which republished a
                // CodexTui handoff against a TUI whose readiness was
                // unknown. Downstream recovery / watcher-relay paths then
                // operated on a non-ready session and ran into the
                // original #2325 failure mode.
                //
                // Updated contract: on a readiness-timeout verdict we
                // suppress `RuntimeReady` entirely. The bridge has already
                // received the rollout-tail `Done` and finalised the
                // assistant text; the only thing we *would* be publishing
                // is the CodexTui handoff metadata. Skipping it forces the
                // bridge to treat the next turn as a fresh session
                // launch (or recovery), which is safer than reusing a
                // possibly-hung pane.
                //
                // Session-dead and cancel cases are already handled by
                // dedicated arms above — only the readiness-timeout path
                // lands here, but we still log the error string verbatim
                // so operators can correlate with the input.rs telemetry.
                tracing::warn!(
                    tmux_session = tmux_session_name,
                    error = %error,
                    "Codex TUI composer not yet input-ready inside post-turn probe budget; suppressing RuntimeReady to avoid republishing a non-ready handoff (#2399 HIGH 2)"
                );
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
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
    developer_instructions: Option<&str>,
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
    let has_live_pane = tmux_session_has_live_pane(tmux_session_name);
    let resolved_output =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "jsonl");
    let resolved_input =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "input");
    let wrapper_input_mode = codex_tmux_wrapper_session_input_mode(tmux_session_name);
    let session_usable = codex_fifo_wrapper_session_usable(
        has_live_pane,
        resolved_output.is_some(),
        wrapper_input_mode,
        resolved_input.is_some(),
    );

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
                record_codex_tmux_termination(
                    tmux_session_name,
                    "codex_provider",
                    "followup_failed_recreate",
                    &format!("followup failed, recreating: {error}"),
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
        }
    } else if codex_input_mode_allows_live_session_preservation(wrapper_input_mode)
        && should_preserve_live_reused_provider_session(
            session_id,
            has_live_pane,
            force_fresh_provider_session,
        )
    {
        // Refuse cleanup for a live wrapper whose input/output files are merely
        // temporarily missing (e.g. a dcserver restart lost /tmp): FIFO-mode
        // panes this build can drive, and Unknown panes whose marker is gone. A
        // live legacy pipe-mode pane is NOT drivable here, so it must fall
        // through to the recreate branch — which resumes the conversation via
        // session id — instead of hard-erroring and stranding the user during a
        // deploy rollover.
        tracing::warn!(
            tmux_session_name,
            session_id = session_id.unwrap_or_default(),
            output_path_present = resolved_output.is_some(),
            input_path_present = resolved_input.is_some(),
            wrapper_input_mode = ?wrapper_input_mode,
            "refusing to kill live Codex tmux selected for provider-session reuse"
        );
        return Err(format!(
            "live Codex tmux session {tmux_session_name} was selected for reuse but wrapper I/O is unavailable; refusing stale cleanup/recreate"
        ));
    } else if session_exists {
        let cleanup_reason =
            codex_wrapper_existing_session_termination_reason(force_fresh_provider_session);
        record_codex_tmux_termination(
            tmux_session_name,
            "codex_provider",
            cleanup_reason.reason_code,
            cleanup_reason.reason_text,
            None,
        );
        record_tmux_exit_reason(tmux_session_name, cleanup_reason.reason_text);
        crate::services::platform::tmux::kill_session(
            tmux_session_name,
            cleanup_reason.reason_text,
        );
    }

    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);
    crate::services::codex_tui::session::CodexTuiSessionFiles::for_tmux_session(tmux_session_name)
        .cleanup_best_effort();

    std::fs::write(&output_path, "").map_err(|e| format!("Failed to create output file: {}", e))?;

    // Create the named input FIFO that the wrapper reads followups from. A FIFO
    // has no terminal line discipline, so the base64 sentinel line we later
    // write is delivered to the wrapper's BufReader::lines() loop unambiguously
    // (unlike PTY paste, which stranded reused prompts — see issue #3001).
    let _ = std::fs::remove_file(&input_fifo_path);
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
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux_session_name,
        crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper,
    )?;

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
        developer_instructions,
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

    // #3087: stamp a per-spawn nonce in a SEPARATE marker (see claude.rs). The
    // status-panel session-instance key reads this unique nonce instead of the
    // `.generation` mtime, eliminating mtime missing/duplicate collisions.
    if let Err(e) = crate::services::discord::write_spawn_nonce(tmux_session_name) {
        tracing::warn!("failed to write spawn nonce for {tmux_session_name}: {e}");
    }

    if let Some(ref token) = cancel_token {
        token.bind_unmanaged_session_name(tmux_session_name);
    }

    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token.clone(),
        SessionProbe::tmux(tmux_session_name.to_string(), ProviderKind::Codex),
    )?;

    match read_result {
        crate::services::provider::ReadOutputResult::Completed { offset }
        | crate::services::provider::ReadOutputResult::Cancelled { offset } => {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        }
        crate::services::provider::ReadOutputResult::SessionDied { offset } => {
            record_codex_tmux_termination(
                tmux_session_name,
                "codex_provider",
                "session_died",
                "codex tmux session ended before turn completion",
                Some(offset),
            );
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
fn codex_pipe_prompt_lines_with_id(prompt: &str, message_id: &str) -> Vec<String> {
    let encoded = BASE64_STANDARD.encode(prompt.as_bytes());
    if encoded.len() <= TMUX_PROMPT_B64_CHUNK_SIZE {
        return vec![format!("{}{}", TMUX_PROMPT_B64_PREFIX, encoded)];
    }

    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(TMUX_PROMPT_B64_CHUNK_SIZE)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64 chunks are ascii"))
        .collect();
    let total = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            format!(
                "{}{}:{}:{}:{}",
                TMUX_PROMPT_B64_CHUNK_PREFIX, message_id, index, total, chunk
            )
        })
        .collect()
}

#[cfg(unix)]
fn codex_pipe_prompt_lines(prompt: &str) -> Vec<String> {
    let message_id = uuid::Uuid::new_v4().simple().to_string();
    codex_pipe_prompt_lines_with_id(prompt, &message_id)
}

#[cfg(unix)]
fn codex_pipe_prompt_buffer_text(prompt: &str) -> String {
    let mut encoded = codex_pipe_prompt_lines(prompt).join("\n");
    encoded.push('\n');
    encoded
}

#[cfg(unix)]
fn send_codex_pipe_prompt_to_fifo(input_fifo_path: &str, prompt: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    // The reused wrapper runs with `--input-mode fifo` and reads followups from
    // a named FIFO via BufReader::lines(). A FIFO has no terminal line
    // discipline, so writing the newline-terminated base64 sentinel line(s)
    // delivers each line unambiguously to the wrapper's decode loop — unlike the
    // old PTY paste path, which stranded the prompt at "Ready for input"
    // because the line discipline never submitted the sentinel (issue #3001).
    let encoded = codex_pipe_prompt_buffer_text(prompt);

    // Open the write side with O_NONBLOCK. The reuse gate observed the FIFO path,
    // but the wrapper (and thus the FIFO reader) can exit between that check and
    // this write while the FIFO file lingers — wrapper error paths preserve
    // session files. A *blocking* O_WRONLY open on a reader-less FIFO would hang
    // forever and never reach the stale-session recreation path below. With
    // O_NONBLOCK the open instead fails fast with ENXIO, which we surface as a
    // recoverable error so the caller can recreate/resume the session (#3001).
    let fifo = match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(input_fifo_path)
    {
        Ok(fifo) => fifo,
        Err(e) => {
            // ENXIO == FIFO has no reader attached (wrapper gone). Normalize the
            // message so should_recreate_session_after_followup_fifo_error()
            // classifies it as recoverable on every platform (macOS reports
            // "Device not configured", Linux "No such device or address").
            if e.raw_os_error() == Some(libc::ENXIO) {
                return Err(format!(
                    "Failed to open input FIFO {input_fifo_path}: No such device (no reader attached: {e})"
                ));
            }
            return Err(format!("Failed to open input FIFO {input_fifo_path}: {e}"));
        }
    };

    // A reader is attached. Clear O_NONBLOCK so the subsequent write blocks
    // normally if the prompt exceeds the kernel pipe buffer (chunked base64 can
    // exceed 64KiB), instead of returning EAGAIN and dropping bytes.
    let fd = fifo.as_raw_fd();
    // SAFETY: fd is a valid, owned descriptor for the lifetime of `fifo`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }

    let mut fifo = fifo;
    fifo.write_all(encoded.as_bytes())
        .map_err(|e| format!("Failed to write to input FIFO {input_fifo_path}: {e}"))?;
    fifo.flush()
        .map_err(|e| format!("Failed to flush input FIFO {input_fifo_path}: {e}"))?;
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

    if let Err(error) = send_codex_pipe_prompt_to_fifo(input_fifo_path, prompt) {
        // The reuse gate passed, but the FIFO/reader can disappear between that
        // check and this write (wrapper exited, cleaned up its FIFO, broken
        // pipe). Treat those infrastructure failures as a stale session and
        // request recreation (which resumes via session id) instead of surfacing
        // a hard provider error — mirroring the Claude/Qwen FIFO follow-up path.
        if should_recreate_session_after_followup_fifo_error(&error) {
            return Ok(FollowupResult::RecreateSession { error });
        }
        return Err(format!(
            "Failed to send Codex follow-up prompt to input FIFO: {error}"
        ));
    }

    if let Some(ref token) = cancel_token {
        token.bind_unmanaged_session_name(tmux_session_name);
    }

    let read_result = match read_output_file_until_result_tracked(
        output_path,
        start_offset,
        sender.clone(),
        cancel_token,
        SessionProbe::tmux_with_structured_output(
            tmux_session_name.to_string(),
            ProviderKind::Codex,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper),
            output_path.to_string(),
        ),
    ) {
        Ok(read_result) => read_result,
        Err(failure) => {
            let output_exists = std::fs::metadata(output_path).is_ok();
            let current_file_len = std::fs::metadata(output_path).ok().map(|meta| meta.len());
            // Codex legacy wrapper runs in tmux fifo mode: follow-ups are
            // written to a named input FIFO, so the FIFO file is the input
            // transport and can be stat'd directly.
            let input_exists = std::fs::metadata(input_fifo_path).is_ok();
            let session_alive = tmux_session_has_live_pane(tmux_session_name);
            let ready_for_input = session_alive
                && crate::services::tui_turn_state::jsonl_ready_for_input(
                    &ProviderKind::Codex,
                    Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
                    std::path::Path::new(output_path),
                    Some(failure.last_offset),
                )
                .is_some_and(crate::services::tui_turn_state::TuiReadyState::is_ready);

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
#[allow(clippy::too_many_arguments)]
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
    developer_instructions: Option<&str>,
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
        .with_developer_instructions(developer_instructions)
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

    register_child_pid(cancel_token.as_deref(), handle.pid());

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
                        let tool_use_id =
                            item.get("id").and_then(|v| v.as_str()).map(str::to_string);
                        let _ = sender.send(StreamMessage::ToolUse {
                            name: "Bash".to_string(),
                            input,
                            tool_use_id,
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
                let tool_use_id = json
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let _ = sender.send(StreamMessage::ToolUse {
                    name,
                    input: codex_mcp_arguments(invocation),
                    tool_use_id,
                });
            }
        }
        "mcp_tool_call_end" => {
            let (content, is_error) = codex_mcp_result(json.get("result").unwrap_or(&Value::Null));
            let tool_use_id = json
                .get("call_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let _ = sender.send(StreamMessage::ToolResult {
                content,
                is_error,
                tool_use_id,
            });
        }
        "background_event" => {
            if let Some(summary) = codex_background_event_summary(&json) {
                let _ = sender.send(StreamMessage::TaskNotification {
                    task_id: CODEX_BACKGROUND_TASK_NOTIFICATION_ID.to_string(),
                    tool_use_id: None,
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
                            // #3431: each codex `agent_message` is a COMPLETE block
                            // (item.completed, never a mid-token delta), so a second+
                            // message must carry the SAME `\n\n` paragraph separator
                            // that `final_text` uses — otherwise the bridge consumer
                            // (`full_response.push_str`) butts two messages together
                            // (`완료했습니다.#3089`). Mirror the separator into the
                            // streamed `Text` so the relayed body matches `final_text`.
                            let separated = if final_text.is_empty() {
                                text.to_string()
                            } else {
                                final_text.push_str("\n\n");
                                format!("\n\n{text}")
                            };
                            final_text.push_str(text);
                            let _ = sender.send(StreamMessage::Text { content: separated });
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
                        let tool_use_id =
                            item.get("id").and_then(|v| v.as_str()).map(str::to_string);
                        let _ = sender.send(StreamMessage::ToolResult {
                            content,
                            is_error,
                            tool_use_id,
                        });
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
        build_codex_tui_args, build_tmux_launch_env_lines,
        codex_resume_help_mentions_hook_trust_bypass, codex_tui_idle_relay_binding,
        direct_tui_material_fallback_reason, insert_codex_resume_option_before_other_options,
        render_codex_tui_tmux_script, render_codex_wrapper_tmux_script,
        should_preserve_live_reused_provider_session, should_reuse_existing_provider_session,
    };
    #[cfg(unix)]
    use super::{
        CodexWrapperInputMode, codex_fifo_wrapper_session_usable,
        codex_input_mode_allows_live_session_preservation,
        codex_tui_existing_session_termination_reason, codex_tui_session_turn_lock,
        codex_wrapper_existing_session_termination_reason, codex_wrapper_script_input_mode,
    };
    use crate::services::discord::restart_report::{
        RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
    };
    use crate::services::provider::ProviderKind;
    use crate::services::provider::ReadOutputResult;

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
    fn codex_tui_idle_relay_binding_is_buildable_without_runtime_ready() {
        let tmux_session_name = format!("AgentDesk-codex-idle-binding-test-{}", std::process::id());
        let rollout_path = std::env::temp_dir().join(format!(
            "agentdesk-codex-idle-binding-test-{}.jsonl",
            std::process::id()
        ));
        let tail_result = crate::services::codex_tui::rollout_tail::CodexTuiTailResult {
            read_result: ReadOutputResult::Completed { offset: 321 },
            rollout_path: rollout_path.clone(),
            final_offset: 321,
            session_id: Some("thread-xyz".to_string()),
        };

        let binding = codex_tui_idle_relay_binding(&tmux_session_name, &tail_result);
        assert_eq!(
            binding.runtime_kind,
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
        );
        assert_eq!(binding.output_path, rollout_path.display().to_string());
        assert_eq!(binding.session_id.as_deref(), Some("thread-xyz"));
        assert_eq!(binding.last_offset, 321);
        assert_eq!(binding.relay_last_offset, Some(0));
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
    fn codex_tui_resume_args_reject_flag_like_session_id() {
        let args = base_tui_args(
            Some("--config"),
            "fresh prompt",
            Some("gpt-5-codex"),
            Some("high"),
            false,
            Some(true),
            None,
        );

        assert!(!args.iter().any(|arg| arg == "resume"));
        assert!(!args.iter().any(|arg| arg == "--config"));
        assert_eq!(args.last().map(String::as_str), Some("fresh prompt"));
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
    fn codex_tui_hook_trust_bypass_is_inserted_before_resume_session_id() {
        let mut args = base_tui_args(
            Some("session-123"),
            "resume prompt",
            Some("gpt-5-codex"),
            None,
            false,
            None,
            None,
        );

        insert_codex_resume_option_before_other_options(
            &mut args,
            "--dangerously-bypass-hook-trust",
        );

        let delimiter_index = args
            .iter()
            .position(|arg| arg == "--")
            .expect("expected prompt delimiter");
        let bypass_index = args
            .iter()
            .position(|arg| arg == "--dangerously-bypass-hook-trust")
            .expect("expected hook trust bypass flag");
        assert_eq!(args[1], "resume");
        assert!(bypass_index < delimiter_index);
        assert!(bypass_index < delimiter_index - 1);
        assert_eq!(bypass_index, 0);
        assert_eq!(args[delimiter_index - 1], "session-123");
    }

    #[test]
    fn codex_tui_hook_trust_bypass_support_is_detected_from_resume_help() {
        assert!(codex_resume_help_mentions_hook_trust_bypass(
            "Usage: codex resume [OPTIONS]\n      --dangerously-bypass-hook-trust\n"
        ));
        assert!(!codex_resume_help_mentions_hook_trust_bypass(
            "Usage: codex resume [OPTIONS]\n      --dangerously-bypass-approvals-and-sandbox\n"
        ));
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
            Some("developer rules"),
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
        assert!(script.contains("'--developer-instructions'"));
        assert!(script.contains("'developer rules'"));
        assert!(script.contains("'--compact-token-limit'"));
        assert!(script.contains("'120000'"));
        assert!(script.contains("--input-mode fifo"));
        // The wrapper script no longer self-creates the FIFO; the parent
        // (execute_streaming_local_tmux) mkfifo's it before launch.
        assert!(!script.contains("mkfifo"));
        assert!(!script.contains("exec '/opt/bin/codex' "));
    }

    #[cfg(unix)]
    #[test]
    fn codex_wrapper_input_mode_detector_distinguishes_fifo_pipe_and_unknown() {
        let fifo_script = "exec agentdesk codex-tmux-wrapper \\\n  --input-mode fifo \\\n";
        let pipe_script = "exec agentdesk codex-tmux-wrapper \\\n  --input-mode pipe \\\n";
        let unknown_script = "exec agentdesk codex-tmux-wrapper \\\n  --output-file /tmp/o \\\n";

        assert_eq!(
            codex_wrapper_script_input_mode(fifo_script),
            CodexWrapperInputMode::Fifo
        );
        assert_eq!(
            codex_wrapper_script_input_mode(pipe_script),
            CodexWrapperInputMode::Pipe
        );
        assert_eq!(
            codex_wrapper_script_input_mode(unknown_script),
            CodexWrapperInputMode::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_fifo_wrapper_reuse_requires_live_pane_output_fifo_mode_and_fifo_file() {
        use CodexWrapperInputMode::{Fifo, Pipe, Unknown};
        // All four conditions must hold: live pane, output transport, explicit
        // fifo input mode, and a resolvable input FIFO to write the follow-up
        // sentinel to.
        assert!(codex_fifo_wrapper_session_usable(true, true, Fifo, true));
        assert!(!codex_fifo_wrapper_session_usable(false, true, Fifo, true));
        assert!(!codex_fifo_wrapper_session_usable(true, false, Fifo, true));
        assert!(!codex_fifo_wrapper_session_usable(true, true, Pipe, true));
        assert!(!codex_fifo_wrapper_session_usable(
            true, true, Unknown, true
        ));
        assert!(!codex_fifo_wrapper_session_usable(true, true, Fifo, false));
    }

    #[cfg(unix)]
    #[test]
    fn codex_live_session_preservation_excludes_only_explicit_pipe_mode() {
        // FIFO-mode (drivable here) and Unknown (marker lost after restart) are
        // preserved; explicit legacy pipe-mode is recreated, not stranded.
        assert!(codex_input_mode_allows_live_session_preservation(
            CodexWrapperInputMode::Fifo
        ));
        assert!(codex_input_mode_allows_live_session_preservation(
            CodexWrapperInputMode::Unknown
        ));
        assert!(!codex_input_mode_allows_live_session_preservation(
            CodexWrapperInputMode::Pipe
        ));
    }

    #[cfg(unix)]
    #[test]
    fn codex_tui_turn_lock_is_shared_only_by_the_same_tmux_session() {
        let first = codex_tui_session_turn_lock("codex-warm-lock-shared");
        let second = codex_tui_session_turn_lock("codex-warm-lock-shared");
        let other = codex_tui_session_turn_lock("codex-warm-lock-other");

        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert!(!std::sync::Arc::ptr_eq(&first, &other));
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

    #[cfg(unix)]
    #[test]
    fn codex_tui_existing_session_audit_reasons_match_cleanup_paths() {
        assert_eq!(
            codex_tui_existing_session_termination_reason(true, true).reason_code,
            "fresh_provider_session_requested"
        );
        assert_eq!(
            codex_tui_existing_session_termination_reason(false, true).reason_code,
            "session_restart_before_direct_launch"
        );
        assert_eq!(
            codex_tui_existing_session_termination_reason(false, false).reason_code,
            "stale_session_recreate"
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_wrapper_existing_session_audit_reasons_match_cleanup_paths() {
        assert_eq!(
            codex_wrapper_existing_session_termination_reason(true).reason_code,
            "fresh_provider_session_requested"
        );
        assert_eq!(
            codex_wrapper_existing_session_termination_reason(false).reason_code,
            "stale_session_recreate"
        );
    }

    #[test]
    fn codex_provider_session_reuse_decision_honors_explicit_fresh_flag() {
        assert!(should_reuse_existing_provider_session(true, false));
        assert!(!should_reuse_existing_provider_session(true, true));
        assert!(!should_reuse_existing_provider_session(false, false));
        assert!(!should_reuse_existing_provider_session(false, true));
    }

    #[test]
    fn live_reused_provider_session_is_preserved_when_wrapper_io_is_missing() {
        assert!(should_preserve_live_reused_provider_session(
            Some("codex-session-1"),
            true,
            false
        ));
        assert!(!should_preserve_live_reused_provider_session(
            Some("codex-session-1"),
            true,
            true
        ));
        assert!(!should_preserve_live_reused_provider_session(
            Some("codex-session-1"),
            false,
            false
        ));
        assert!(!should_preserve_live_reused_provider_session(
            Some("  "),
            true,
            false
        ));
        assert!(!should_preserve_live_reused_provider_session(
            None, true, false
        ));
    }
}

#[cfg(test)]
mod codex_fifo_followup_transport_tests {
    #[cfg(unix)]
    #[test]
    fn reused_fifo_wrapper_buffer_keeps_protocol_line_terminated() {
        let buffer = super::codex_pipe_prompt_buffer_text("[E2E:E2:TURN-2]\nreply json");

        assert!(buffer.starts_with(super::TMUX_PROMPT_B64_PREFIX));
        assert!(
            buffer.ends_with('\n'),
            "the FIFO protocol line must be newline-terminated so the wrapper's BufReader::lines() loop submits it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn send_codex_pipe_prompt_writes_sentinel_to_fifo() {
        use std::io::Read;

        let dir = std::env::temp_dir().join(format!(
            "agentdesk-codex-fifo-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let fifo_path = dir.join("input");
        let fifo_path_str = fifo_path.to_string_lossy().to_string();

        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo should succeed");

        let prompt = "[E2E:E2:TURN-2]\nreply json";
        let expected = super::codex_pipe_prompt_buffer_text(prompt);

        // In production the codex wrapper holds the input FIFO open `O_RDWR`, so
        // the reused-wrapper write side — which opens `O_NONBLOCK` and fails fast
        // with ENXIO when no reader is attached (see `send_codex_pipe_prompt_to_fifo`)
        // — always finds a reader. Mirror that here by holding a reader fd open
        // BEFORE writing; a separate reader thread races the non-blocking open and
        // fails ENXIO. `O_RDWR` attaches a reader without blocking and without an
        // EOF race, so read exactly the bytes we expect.
        let mut reader = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo_path)
            .expect("open fifo O_RDWR to attach a reader");

        super::send_codex_pipe_prompt_to_fifo(&fifo_path_str, prompt)
            .expect("write follow-up sentinel to FIFO");

        let mut buf = vec![0u8; expected.len()];
        reader.read_exact(&mut buf).expect("read fifo");
        let received = String::from_utf8(buf).expect("fifo bytes are utf-8");
        assert_eq!(
            received, expected,
            "the wrapper must receive the exact newline-terminated base64 sentinel via the FIFO"
        );
        assert!(received.ends_with('\n'));

        let _ = std::fs::remove_dir_all(&dir);
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

#[cfg(test)]
mod relay_separator_tests {
    use super::handle_codex_json_line;
    use crate::services::agent_protocol::StreamMessage;

    fn agent_message_line(text: &str) -> String {
        format!(r#"{{"type":"item.completed","item":{{"type":"agent_message","text":"{text}"}}}}"#)
    }

    #[test]
    fn codex_second_agent_message_carries_paragraph_separator_3431() {
        // #3431: the bridge accumulates streamed `Text` via `push_str` with NO
        // separator, so a 2nd+ codex agent_message must itself carry the `\n\n`
        // paragraph separator that `final_text` uses — otherwise two complete
        // messages butt together in the relayed body (`완료했습니다.#3089`). The
        // FIRST message emits raw text (no leading separator); the SECOND carries
        // a `\n\n` prefix. `final_text` keeps its own joined form unchanged.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started = std::time::Instant::now();

        handle_codex_json_line(
            &agent_message_line("first"),
            &tx,
            &mut thread_id,
            &mut final_text,
            started,
        )
        .unwrap();
        handle_codex_json_line(
            &agent_message_line("second"),
            &tx,
            &mut thread_id,
            &mut final_text,
            started,
        )
        .unwrap();
        drop(tx);

        let texts: Vec<String> = rx
            .iter()
            .filter_map(|m| match m {
                StreamMessage::Text { content } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["first".to_string(), "\n\nsecond".to_string()]);
        assert_eq!(final_text, "first\n\nsecond");
    }
}
