//! Tui session launch.

#[cfg(unix)]
mod launch_policy;
#[cfg(unix)]
pub(crate) use launch_policy::successor_hooks_ready;
#[cfg(unix)]
pub(super) use launch_policy::{
    preflight_codex_tui_turn_with_policy, prepare_codex_tui_incarnation_with_policy,
    tui_preflight_with_policy,
};

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CodexTuiLaunchRefused {
    SourceModeInvalid(&'static str),
    HooksUnavailable,
    CliMissing,
    UnsupportedOptions,
    UnprovedPane,
    ResumeUnavailable,
    PreparationUnavailable(String),
    HerdrNotAdmitted,
}

#[cfg(unix)]
impl std::fmt::Display for CodexTuiLaunchRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceModeInvalid(reason) => write!(f, "CodexTuiNotAttempted:{reason}"),
            Self::PreparationUnavailable(reason) => write!(f, "CodexTuiNotAttempted:{reason}"),
            Self::HerdrNotAdmitted => {
                f.write_str(crate::services::herdr_launch::HERDR_NOT_ADMITTED)
            }
            other => write!(f, "CodexTuiNotAttempted:{other:?}"),
        }
    }
}

/// Read-only eligibility, reused immediately before each pane effect.
#[cfg(unix)]
pub(crate) fn tui_preflight(
    tmux: &str,
    channel: Option<u64>,
    options: &CodexLaunchOptions,
    resume_root: Option<&std::path::Path>,
) -> Result<&'static str, CodexTuiLaunchRefused> {
    tui_preflight_with_policy(tmux, channel, options, resume_root, false)
}

/// A warm incarnation keeps its verified policy when the next launch mode changes.
#[cfg(unix)]
pub(crate) fn verified_warm_preflight<'a>(
    options: &CodexLaunchOptions,
    context: &'a crate::services::tui_prompt_dedupe::binding_context::BindingContext,
) -> Result<VerifiedWarmEligibility<'a>, CodexTuiLaunchRefused> {
    let resume_root = (context.launch_mode == "resume"
        && context.expected_native_session_id.as_deref() == options.resume_session_id.as_deref())
    .then_some(context.provider_root.as_deref())
    .flatten();
    let eligibility = VerifiedWarmEligibility {
        resume_root,
        hooks: verified_eligibility(options, resume_root)?,
    };
    eligibility.recheck()?;
    Ok(eligibility)
}

#[cfg(unix)]
pub(crate) struct VerifiedWarmEligibility<'a> {
    resume_root: Option<&'a std::path::Path>,
    hooks: bool,
}

#[cfg(unix)]
impl VerifiedWarmEligibility<'_> {
    /// Cheap live reads follow the external probe and the other effect guards.
    pub(crate) fn recheck(&self) -> Result<(), CodexTuiLaunchRefused> {
        if verified_hook_eligibility(self.resume_root)? != self.hooks {
            return Err(CodexTuiLaunchRefused::HooksUnavailable);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn verified_hook_eligibility(
    resume_root: Option<&std::path::Path>,
) -> Result<bool, CodexTuiLaunchRefused> {
    let hooks = codex_direct_tui_hook_overrides_enabled();
    if (!hooks && resume_root.is_none()) || (hooks && current_hook_endpoint().is_none()) {
        return Err(CodexTuiLaunchRefused::HooksUnavailable);
    }
    Ok(hooks)
}

#[cfg(unix)]
fn verified_eligibility(
    options: &CodexLaunchOptions,
    resume_root: Option<&std::path::Path>,
) -> Result<bool, CodexTuiLaunchRefused> {
    if direct_tui_material_fallback_reason(options).is_some() {
        return Err(CodexTuiLaunchRefused::UnsupportedOptions);
    }
    if let Some(root) = resume_root {
        use crate::services::codex_tui::session::source_observation::{
            CodexHookSourceClaim, CodexRolloutSource, verify_codex_hook_source,
        };
        let id = options
            .resume_session_id
            .as_deref()
            .filter(|id| exact_codex_resume_id(id))
            .ok_or(CodexTuiLaunchRefused::ResumeUnavailable)?;
        verify_codex_hook_source(
            root,
            &CodexHookSourceClaim {
                session_id: id,
                transcript_path: None,
                expected_source: CodexRolloutSource::Cli,
            },
        )
        .map_err(|_| CodexTuiLaunchRefused::ResumeUnavailable)?;
    }
    let hooks = verified_hook_eligibility(resume_root)?;
    let resolution = resolve_codex_binary();
    let binary = resolution
        .resolved_path
        .as_deref()
        .ok_or(CodexTuiLaunchRefused::CliMissing)?;
    if hooks && !codex_resume_supports_hook_trust_bypass(binary, &resolution) {
        return Err(CodexTuiLaunchRefused::HooksUnavailable);
    }
    Ok(hooks)
}

#[cfg(unix)]
fn exact_codex_resume_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.hyphenated().to_string() == id)
}

#[cfg(unix)]
fn codex_tui_home(
    overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
) -> (Option<PathBuf>, bool) {
    let pinned = overlay.env.get("CODEX_HOME").map(PathBuf::from);
    let unpinned = pinned.is_none() && !overlay.unset.contains("CODEX_HOME");
    let home = match pinned {
        Some(home) => Some(home),
        None if unpinned => crate::services::codex_tui::rollout_tail::default_codex_home(),
        None => dirs::home_dir().map(|home| home.join(".codex")),
    };
    (home, unpinned)
}

#[cfg(all(test, unix))]
pub(super) fn prepare_codex_tui_incarnation(
    tmux: &str,
    channel: Option<u64>,
    options: &CodexLaunchOptions,
    overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
    resume_root: Option<&std::path::Path>,
) -> Result<PreparedIncarnation, CodexTuiLaunchRefused> {
    prepare_codex_tui_incarnation_with_policy(tmux, channel, options, overlay, resume_root, false)
}

#[cfg(all(test, unix))]
pub(super) fn preflight_codex_tui_turn(
    tmux: &str,
    channel: Option<u64>,
    options: &CodexLaunchOptions,
    selection: &crate::services::codex_tui::session::CodexTuiSessionSelection,
    existing: (bool, bool),
    overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
) -> Result<(Option<PreparedIncarnation>, Option<PathBuf>), CodexTuiLaunchRefused> {
    preflight_codex_tui_turn_with_policy(
        tmux, channel, options, selection, existing, overlay, false,
    )
}

#[cfg(all(test, unix))]
thread_local! {
    pub(super) static BEFORE_PANE_EFFECT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
fn prepare_codex_tui_launch_script(
    tmux: &str,
    session_id: Option<&str>,
    prompt: &str,
    options: &CodexLaunchOptions,
    report: (Option<u64>, Option<ProviderKind>),
    warm: bool,
    overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
) -> Result<CodexTuiLaunchScript, String> {
    let resume_root = options
        .resume_session_id
        .as_deref()
        .filter(|id| exact_codex_resume_id(id))
        .and_then(|_| codex_tui_home(overlay).0.map(|home| home.join("sessions")));
    let prepared =
        prepare_codex_tui_incarnation(tmux, report.0, options, overlay, resume_root.as_deref())
            .map_err(|error| error.to_string())?;
    publish_codex_tui_launch_script(
        tmux,
        session_id,
        prompt,
        options,
        report,
        warm,
        (overlay, prepared),
    )
}

/// Publish markers and script after the driver completes its cleanup.
#[cfg(unix)]
pub(super) fn publish_codex_tui_launch_script(
    tmux_session_name: &str,
    session_id: Option<&str>,
    prompt: &str,
    launch_options: &CodexLaunchOptions,
    report: (Option<u64>, Option<ProviderKind>),
    warm_followup_enabled: bool,
    launch: (
        &crate::services::provider_auth_profile::ProviderAuthOverlay,
        PreparedIncarnation,
    ),
) -> Result<CodexTuiLaunchScript, String> {
    let (report_channel_id, report_provider) = report;
    use crate::services::herdr_launch::{HERDR_NOT_ADMITTED, herdr_configured_for_tui_launch};
    // Refused before any launch I/O; a Herdr channel never gets a tmux session.
    if herdr_configured_for_tui_launch(report_channel_id) {
        return Err(HERDR_NOT_ADMITTED.to_string());
    }
    let (auth_overlay, prepared) = launch;
    let policy = prepared
        .context
        .source_policy
        .as_deref()
        .ok_or("missing launch policy")?;
    write_tmux_owner_marker(tmux_session_name)?;
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux_session_name,
        crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
    )?;
    let owner_path = tmux_owner_path(tmux_session_name);

    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");
    // The auth profile decides the child's home; an unpinned one is exported only with hooks.
    let (codex_home, unpinned) = codex_tui_home(auth_overlay);
    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;
    let mut env_lines = build_tmux_launch_env_lines(
        resolution.exec_path.as_deref(),
        report_channel_id,
        report_provider,
    );
    env_lines
        .push_str(&crate::services::provider_auth_profile::overlay_shell_env_lines(auth_overlay));
    env_lines.push_str(&prepared.env_lines());
    env_lines.push_str(&format!(
        "export AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE={}\n",
        shell_escape(policy)
    ));
    let mut args = build_codex_tui_args(launch_options);
    let hooks_enabled = codex_direct_tui_hook_overrides_enabled();
    let hooks_injected = hooks_enabled
        && {
            let capability = crate::services::claude_tui::hook_bundle::codex_hook_capability(
                crate::services::claude_tui::hook_bundle::probe_codex_cli_version_with_path(
                    &codex_bin,
                    resolution.exec_path.as_deref(),
                )
                .as_deref(),
                codex_resume_supports_hook_trust_bypass(&codex_bin, &resolution),
            );
            if !capability.hooks_available() {
                tracing::warn!(
                    codex_bin,
                    trust_hash = ?capability.trust_hash,
                    "Codex resume does not advertise --dangerously-bypass-hook-trust; launching without hook relays"
                );
            }
            add_codex_tui_hooks(&mut args, capability, || {
                prepare_codex_tui_hook_overrides(
                    tmux_session_name,
                    session_id,
                    &codex_bin,
                    resolution.exec_path.as_deref(),
                )
            })
        };
    if policy == "verified"
        && !hooks_injected
        && (hooks_enabled
            || !launch_options
                .resume_session_id
                .as_deref()
                .is_some_and(exact_codex_resume_id))
    {
        return Err(CodexTuiLaunchRefused::HooksUnavailable.to_string());
    }
    if !hooks_injected {
        tracing::info!(
            tmux_session_name,
            "Codex direct TUI session hook overrides not injected; using rollout transcript tail for relay"
        );
    }
    if unpinned
        && (hooks_injected || policy == "verified")
        && let Some(home) = &codex_home
    {
        // Verified sources must use the recorded home, including resumes without hooks.
        env_lines.push_str(&format!(
            "export CODEX_HOME={}\n",
            shell_escape(&home.to_string_lossy())
        ));
    }
    let script_content = render_codex_tui_tmux_script(&env_lines, &codex_bin, &args);
    let rollout_modified_since = std::time::SystemTime::now();

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write Codex TUI launch script: {}", e))?;
    if warm_followup_enabled {
        crate::services::codex_tui::warm_followup::write_codex_tui_launch_options_evidence(
            tmux_session_name,
            launch_options,
        )?;
    }
    // The launch's tail holds this until it returns.
    let handoff = crate::services::tui_prompt_dedupe::reserve_discord_handoff(
        ProviderKind::Codex.as_str(),
        tmux_session_name,
        prompt,
    );
    crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
        ProviderKind::Codex.as_str(),
        tmux_session_name,
        prompt,
    );
    Ok(CodexTuiLaunchScript {
        prepared,
        script_path,
        owner_path,
        rollout_modified_since,
        handoff,
    })
}

/// Why a Codex Herdr launch is refused, judged from reads before its Pending row.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexHerdrLaunchRefused {
    CliMissing,
    SourceModeInvalid(&'static str),
    /// Without hooks no Source of the launch is ever recorded, so its first prompt has no commit.
    HooksUnavailable,
    /// Without `--no-daemon` a shared app-server keeps the first launch's binding context.
    DaemonIsolationUnavailable,
}

/// What an admitted Codex Herdr launch runs: the binary, its source mode and its hook endpoint.
#[cfg(unix)]
pub(super) struct CodexHerdrLaunchable {
    codex_bin: String,
    resolution: crate::services::platform::BinaryResolution,
    policy: &'static str,
}

/// Both hooks and daemon isolation are required, read from the binary's `--version` and
/// `resume --help` before anything is written.
#[cfg(unix)]
pub(super) fn codex_herdr_launch_admissible()
-> Result<CodexHerdrLaunchable, CodexHerdrLaunchRefused> {
    use crate::services::claude_tui::hook_bundle::{
        codex_hook_capability, probe_codex_cli_version_with_path,
    };
    // Herdr launches are outside the tmux canary.
    let policy = crate::services::codex_tui::canary::launch_policy("", None)
        .map_err(CodexHerdrLaunchRefused::SourceModeInvalid)?;
    let resolution = resolve_codex_binary();
    let codex_bin = resolution.resolved_path.clone();
    let codex_bin = codex_bin.ok_or(CodexHerdrLaunchRefused::CliMissing)?;
    if !codex_direct_tui_hook_overrides_enabled() || current_hook_endpoint().is_none() {
        return Err(CodexHerdrLaunchRefused::HooksUnavailable);
    }
    let help = codex_resume_help(&codex_bin, &resolution).unwrap_or_default();
    let version = probe_codex_cli_version_with_path(&codex_bin, resolution.exec_path.as_deref());
    let bypass = codex_resume_help_mentions_hook_trust_bypass(&help);
    if !codex_hook_capability(version.as_deref(), bypass).hooks_available() {
        return Err(CodexHerdrLaunchRefused::HooksUnavailable);
    }
    if !help.contains("--no-daemon") {
        return Err(CodexHerdrLaunchRefused::DaemonIsolationUnavailable);
    }
    Ok(CodexHerdrLaunchable {
        codex_bin,
        resolution,
        policy,
    })
}

/// The Codex home a Herdr launch exports and its hooks verify sources under: the auth
/// profile's, else this host's.
#[cfg(unix)]
pub(super) fn codex_herdr_home(
    auth_overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
) -> Option<PathBuf> {
    match auth_overlay.env.get("CODEX_HOME") {
        Some(home) => Some(PathBuf::from(home)),
        None => crate::services::codex_tui::rollout_tail::default_codex_home(),
    }
}

/// The Herdr pane command of one prepared incarnation: no prompt in the argv, `--no-daemon`,
/// hooks and an exported home, with the Herdr pane variables unset right before the exec.
#[cfg(unix)]
pub(super) fn prepare_codex_herdr_launch(
    prepared: &PreparedIncarnation,
    launchable: &CodexHerdrLaunchable,
    launch_options: &CodexLaunchOptions,
    auth_overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
    home: &std::path::Path,
) -> Result<crate::services::herdr_launch::HerdrLaunchCommand, String> {
    use crate::services::herdr_launch::{HerdrLaunchCommand, unset_herdr_env_before_exec};
    let logical = prepared.context.tmux_session.as_str();
    let (codex_bin, exec_path) = (
        &launchable.codex_bin,
        launchable.resolution.exec_path.as_deref(),
    );
    let channel = prepared.context.channel_id;
    let mut env_lines = build_tmux_launch_env_lines(exec_path, channel, Some(ProviderKind::Codex));
    env_lines
        .push_str(&crate::services::provider_auth_profile::overlay_shell_env_lines(auth_overlay));
    env_lines.push_str(&prepared.env_lines());
    env_lines.push_str(&format!(
        "export AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE={}\nexport CODEX_HOME={}\n",
        shell_escape(launchable.policy),
        shell_escape(&home.to_string_lossy())
    ));
    let mut args = codex_tui_option_args(launch_options);
    args.insert(0, "--no-daemon".to_string());
    let overrides = prepare_codex_tui_hook_overrides(logical, None, codex_bin, exec_path);
    if overrides.is_empty() {
        return Err("codex herdr launch: no hook overrides".into());
    }
    append_codex_config_overrides(&mut args, overrides);
    insert_codex_resume_option_before_other_options(&mut args, "--dangerously-bypass-hook-trust");
    let script = render_codex_tui_tmux_script(&env_lines, codex_bin, &args);
    let script = unset_herdr_env_before_exec(&script)?;
    let path = crate::services::tmux_common::session_temp_path(logical, "sh");
    std::fs::write(&path, script)
        .map_err(|error| format!("Failed to write Codex Herdr launch script: {error}"))?;
    let cwd = launch_options.cwd.as_deref().map(PathBuf::from);
    Ok(HerdrLaunchCommand {
        cwd: cwd.ok_or("codex herdr launch: no working directory")?,
        command: format!("bash {}", shell_escape(&path)),
    })
}

fn codex_resume_supports_hook_trust_bypass(
    codex_bin: &str,
    resolution: &crate::services::platform::BinaryResolution,
) -> bool {
    codex_resume_help(codex_bin, resolution)
        .is_some_and(|help| codex_resume_help_mentions_hook_trust_bypass(&help))
}

/// `codex resume --help` within two seconds, when it exits successfully.
fn codex_resume_help(
    codex_bin: &str,
    resolution: &crate::services::platform::BinaryResolution,
) -> Option<String> {
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
                            return None;
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
                        return None;
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
            status.success().then_some(help_text)
        }
        Err(error) => {
            tracing::warn!(
                codex_bin,
                error = %error,
                "could not inspect Codex resume help for hook trust bypass support"
            );
            None
        }
    }
}

/// Hook overrides enter the argv only together with the trust bypass; trust hashes alone never do.
fn add_codex_tui_hooks(
    args: &mut Vec<String>,
    capability: crate::services::claude_tui::hook_bundle::CodexHookCapability,
    overrides: impl FnOnce() -> Vec<String>,
) -> bool {
    if !capability.hooks_available() {
        return false;
    }
    let overrides = overrides();
    if overrides.is_empty() {
        return false;
    }
    append_codex_config_overrides(args, overrides);
    insert_codex_resume_option_before_other_options(args, "--dangerously-bypass-hook-trust");
    true
}

/// Publishes a tail handoff only after cancellation, source and readiness checks.
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
    if !crate::services::tui_prompt_dedupe::codex_verified_source_allowed(
        tmux_session_name,
        &tail_result.rollout_path.display().to_string(),
        tail_result.session_id.as_deref(),
    ) {
        return verified_hold::wait_for_cancel(
            tmux_session_name,
            cancel_token_for_post_tail.as_ref(),
        );
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
        if !register_codex_tui_idle_relay_binding(tmux_session_name, &tail_result) {
            return Ok(());
        }

        match crate::services::codex_tui::input::wait_until_codex_tui_input_ready(
            tmux_session_name,
            crate::services::codex_tui::input::PromptReadinessKind::PostTurnHandoff,
            cancel_token_for_post_tail.as_ref(),
        ) {
            Ok(()) => {
                #[cfg(test)]
                if let Some(seam) = AFTER_READINESS_WAIT.with_borrow_mut(Option::take) {
                    seam();
                }
                let ready = StreamMessage::RuntimeReady {
                    handoff: RuntimeHandoff::CodexTui {
                        rollout_path: tail_result.rollout_path.display().to_string(),
                        thread_id: tail_result.session_id.clone(),
                        tmux_session_name: tmux_session_name.to_string(),
                        last_offset: tail_result.final_offset,
                    },
                };
                if !codex_direct_tui_hook_overrides_enabled() {
                    let _ = sender.send(ready);
                } else if !crate::services::tui_prompt_dedupe::publish_unless_codex_tail_retired(
                    &codex_tui_idle_relay_binding(tmux_session_name, &tail_result),
                    tmux_session_name,
                    || drop(sender.send(ready)),
                ) {
                    tracing::info!(
                        tmux_session = tmux_session_name,
                        "Codex tail source was replaced during the readiness wait; suppressing RuntimeReady"
                    );
                }
            }
            Err(error)
                if crate::services::codex_tui::input::is_prompt_ready_cancelled_error(&error) =>
            {
                tracing::info!(
                    tmux_session = tmux_session_name,
                    "Codex TUI input readiness wait cancelled post-turn; suppressing RuntimeReady"
                );
                return Ok(());
            }
            Err(error) if crate::services::codex_tui::input::is_session_dead_error(&error) => {
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

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    #[test]
    fn exact_verified_resume_with_hooks_off_requires_unique_file_proof_and_pins_the_child_home() {
        use crate::config::TestEnvVarGuard as Guard;
        use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
        use crate::services::codex_tui::session::resolve_codex_tui_session;
        use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context};
        use std::os::unix::fs::PermissionsExt;
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
        let _tmux = binding_context::tests::fake_tmux(root.path());
        let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
        let home = root.path().join("codex-home");
        let sessions = home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let _home = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &home);
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let native = sessions.join(format!("rollout-{id}.jsonl"));
        let header = serde_json::json!({"type":"session_meta", "timestamp":chrono::Utc::now().to_rfc3339(),
            "payload":{"id":id, "timestamp":chrono::Utc::now().to_rfc3339(), "cwd":root.path(), "source":"cli", "originator":"codex_cli_rs"}});
        std::fs::write(&native, format!("{header}\n")).unwrap();
        let before = std::fs::read(&native).unwrap();
        let cli = root.path().join("codex");
        std::fs::write(&cli, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.157.1'; elif [ \"$1 $2\" = 'resume --help' ]; then echo unsupported; else printf 'home=%s\\n' \"$CODEX_HOME\"; printf 'arg=%s\\n' \"$@\"; fi\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _cli = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &cli);
        let _hooks = Guard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
            std::ffi::OsStr::new("0"),
        );
        let selection = resolve_codex_tui_session(Some(&id), root.path(), None, false);
        assert_eq!(selection.candidate_count, 1);
        let options =
            CodexLaunchOptions::new("new resume prompt").with_resume_session_id(Some(&id));
        let overlay = crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
            ProviderKind::Codex,
        );
        SOURCE_MODE_TEST.with(|m| m.set(Some(CodexSourceMode::Verified)));
        let (prepared, resume_root) = preflight_codex_tui_turn(
            CANARY_TMUX,
            Some(CANARY_CHANNEL),
            &options,
            &selection,
            (true, false),
            &overlay,
        )
        .unwrap();
        let prepared = prepared.unwrap();
        assert_eq!(resume_root, Some(sessions.clone()));
        assert_eq!(
            prepared.context.expected_native_session_id.as_deref(),
            Some(id.as_str())
        );
        assert_eq!(prepared.context.launch_mode, "resume");
        verified_warm_preflight(&options, &prepared.context)
            .unwrap()
            .recheck()
            .unwrap();
        let mut fresh = prepared.context.clone();
        fresh.launch_mode = "fresh".into();
        assert!(matches!(
            verified_warm_preflight(&options, &fresh),
            Err(CodexTuiLaunchRefused::HooksUnavailable)
        ));
        fresh.launch_mode = "resume".into();
        fresh.expected_native_session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(matches!(
            verified_warm_preflight(&options, &fresh),
            Err(CodexTuiLaunchRefused::HooksUnavailable)
        ));
        assert!(
            binding_context::codex_context_candidate(&prepared.context, &prepared.context, None)
                .is_err(),
            "unused prepared evidence grants no current binding"
        );
        let script = publish_codex_tui_launch_script(
            CANARY_TMUX,
            Some(&id),
            "new resume prompt",
            &options,
            (Some(CANARY_CHANNEL), None),
            false,
            (&overlay, prepared),
        )
        .unwrap();
        let output = std::process::Command::new("/bin/bash")
            .arg(&script.script_path)
            .env("CODEX_HOME", root.path().join("foreign-home"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(&format!("home={}\n", home.display())));
        assert!(stdout.contains(&format!("arg={id}\n")));
        assert!(!stdout.contains("--dangerously-bypass-hook-trust"));
        assert_eq!(std::fs::read(&native).unwrap(), before);
        let duplicate = sessions.join("duplicate");
        std::fs::create_dir(&duplicate).unwrap();
        std::fs::write(duplicate.join(format!("rollout-{id}.jsonl")), &before).unwrap();
        assert!(matches!(
            tui_preflight(CANARY_TMUX, Some(CANARY_CHANNEL), &options, Some(&sessions)),
            Err(CodexTuiLaunchRefused::ResumeUnavailable)
        ));
        let mut ambiguous = selection.clone();
        ambiguous.candidate_count = 2;
        assert!(
            preflight_codex_tui_turn(
                CANARY_TMUX,
                Some(CANARY_CHANNEL),
                &options,
                &ambiguous,
                (true, false),
                &overlay
            )
            .is_err()
        );
        let mismatch = CodexLaunchOptions::new("unused").with_resume_session_id(Some("not-a-uuid"));
        assert!(
            preflight_codex_tui_turn(
                CANARY_TMUX,
                Some(CANARY_CHANNEL),
                &mismatch,
                &selection,
                (true, false),
                &overlay
            )
            .is_err()
        );
        SOURCE_MODE_TEST.with(|m| m.set(None));
        dedupe::reset_state_for_tests();
    }
    #[test]
    fn verified_launch_archives_the_exact_context_without_prompt_or_binding_authority() {
        use crate::config::TestEnvVarGuard as Guard;
        use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
        use crate::services::tui_prompt_dedupe::binding_context;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let (root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
        let _tmux = binding_context::tests::fake_tmux(root.path());
        let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
        let _endpoint_lock = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:9".into(),
        );
        let binary = root.path().join("codex");
        std::fs::write(&binary, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.157.1'; else echo '--dangerously-bypass-hook-trust'; fi\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &binary);
        let _hooks = Guard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
            std::ffi::OsStr::new("1"),
        );
        SOURCE_MODE_TEST.with(|m| m.set(Some(CodexSourceMode::Verified)));
        let script = prepare_codex_tui_launch_script(
            CANARY_TMUX,
            None,
            "secret raw prompt",
            &CodexLaunchOptions::new("secret raw prompt"),
            (Some(CANARY_CHANNEL), None),
            false,
            &crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
                ProviderKind::Codex,
            ),
        )
        .unwrap();
        SOURCE_MODE_TEST.with(|m| m.set(None));
        let p = script.prepared;
        let archive = root
            .path()
            .join("runtime/binding_evidence/codex")
            .join(format!("{}.json", p.context.execution_nonce));
        assert!(
            archive.is_file(),
            "verified launch must durably archive before publication"
        );
        let canonical_meta = std::fs::metadata(&p.path).unwrap();
        let archive_meta = std::fs::metadata(&archive).unwrap();
        assert_eq!(
            (canonical_meta.dev(), canonical_meta.ino()),
            (archive_meta.dev(), archive_meta.ino())
        );
        let bytes = std::fs::read(&archive).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("secret raw prompt"));
        assert_eq!(
            serde_json::from_slice::<binding_context::BindingContext>(&bytes).unwrap(),
            p.context
        );
        std::fs::remove_file(&p.path).unwrap();
        assert_eq!(
            std::fs::read(&archive).unwrap(),
            bytes,
            "old binary canonical sweep leaves archive intact"
        );
        assert!(binding_context::execution_context("codex", &p.context.execution_nonce).is_err());
        assert!(binding_context::input_context("codex", &p.context.execution_nonce).is_none());
    }
    #[test]
    fn actual_launch_pins_verified_only_for_exact_canary_and_keeps_policy_after_rollback() {
        use crate::config::TestEnvVarGuard as Guard;
        use crate::services::codex_tui::canary::{self, CANARY_CHANNEL, CANARY_TMUX};
        use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context};
        use std::os::unix::fs::PermissionsExt;

        struct SourceModeRestore(Option<CodexSourceMode>);
        impl Drop for SourceModeRestore {
            fn drop(&mut self) {
                SOURCE_MODE_TEST.with(|slot| slot.set(self.0));
            }
        }
        let _mode = SourceModeRestore(SOURCE_MODE_TEST.with(|slot| slot.get()));
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _endpoint_lock = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:9".into(),
        );
        let _state = dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
        let _tmux = binding_context::tests::fake_tmux(root.path());
        let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
        let binary = root.path().join("codex");
        std::fs::write(&binary, "#!/bin/bash\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.157.1'; else echo '--dangerously-bypass-hook-trust'; fi\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &binary);
        let _hooks = Guard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
            std::ffi::OsStr::new("0"),
        );

        for mode in [
            CodexSourceMode::Legacy,
            CodexSourceMode::Shadow,
            CodexSourceMode::Verified,
        ] {
            SOURCE_MODE_TEST.with(|slot| slot.set(Some(mode)));
            assert!(matches!(
                codex_herdr_launch_admissible(),
                Err(CodexHerdrLaunchRefused::HooksUnavailable)
            ));
            for (tmux, channel) in [
                (CANARY_TMUX, Some(CANARY_CHANNEL)),
                (CANARY_TMUX, Some(CANARY_CHANNEL + 1)),
                (CANARY_TMUX, None),
                ("AgentDesk-codex-another-channel", Some(CANARY_CHANNEL)),
                ("AgentDesk-codex-another-channel", Some(CANARY_CHANNEL + 1)),
            ] {
                let expected = match mode {
                    CodexSourceMode::Verified
                        if tmux == CANARY_TMUX && channel == Some(CANARY_CHANNEL) =>
                    {
                        "verified"
                    }
                    CodexSourceMode::Shadow => "shadow",
                    _ => "legacy",
                };
                let _verified_hooks = (expected == "verified").then(|| {
                    Guard::set_value_after_shared_test_env_lock(
                        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
                        std::ffi::OsStr::new("1"),
                    )
                });
                let script = prepare_codex_tui_launch_script(
                    tmux,
                    None,
                    "actual first prompt",
                    &CodexLaunchOptions::new("actual first prompt"),
                    (channel, None),
                    false,
                    &crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
                        ProviderKind::Codex,
                    ),
                )
                .unwrap();
                let context = script.prepared.context.clone();
                assert_eq!(context.source_policy.as_deref(), Some(expected));
                assert_eq!(context.channel_id, channel);
                assert_eq!(context.tmux_session, tmux);
                let launch_script = std::fs::read_to_string(&script.script_path).unwrap();
                assert!(launch_script.contains(&format!(
                    "export AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE={}\n",
                    shell_escape(expected),
                )));
                SOURCE_MODE_TEST.with(|slot| slot.set(Some(CodexSourceMode::Legacy)));
                assert_eq!(
                    binding_context::execution_context("codex", &context.execution_nonce).unwrap(),
                    context
                );
                SOURCE_MODE_TEST.with(|slot| slot.set(Some(mode)));
            }
            assert_eq!(
                canary::enabled_for(CANARY_TMUX),
                mode == CodexSourceMode::Verified
            );
            assert!(!canary::enabled_for("AgentDesk-codex-another-channel"));
        }
        SOURCE_MODE_TEST.with(|slot| slot.set(Some(CodexSourceMode::Invalid)));
        assert_eq!(
            canary::launch_policy(CANARY_TMUX, Some(CANARY_CHANNEL)),
            Err("SourceModeInvalid")
        );
        assert_eq!(
            CodexSourceMode::Verified.launch_policy(),
            Err("SourceModeVerifiedNotLanded")
        );
    }

    #[test]
    fn source_mode_snapshot_is_immutable_and_invalid_launches_write_nothing() {
        if std::env::var_os("ADK_SOURCE_SNAPSHOT_CHILD").is_some() {
            assert_eq!(codex_source_mode_snapshot(), CodexSourceMode::Shadow);
            unsafe {
                std::env::set_var("AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE", "legacy");
            }
            assert_eq!(codex_source_mode_snapshot(), CodexSourceMode::Shadow);
            return;
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "services::codex::tui_session_launch::tests::source_mode_snapshot_is_immutable_and_invalid_launches_write_nothing"])
            .env("ADK_SOURCE_SNAPSHOT_CHILD","1").env("AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE","shadow").output().unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stdout)
        );
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));
        use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests};
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (root, _guards) = tests::fixture_after_shared_test_env_lock();
        let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
        for (value, reason) in [
            (Some("typo"), "SourceModeInvalid"),
            (Some(""), "SourceModeInvalid"),
            (Some("SHADOW"), "SourceModeInvalid"),
        ] {
            let mode = CodexSourceMode::parse(value);
            SOURCE_MODE_TEST.with(|m| m.set(Some(mode)));
            let result = prepare_codex_tui_launch_script(
                "source-mode-held",
                None,
                "unused",
                &CodexLaunchOptions::new("actual argv"),
                (None, None),
                false,
                &crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
                    ProviderKind::Codex,
                ),
            );
            SOURCE_MODE_TEST.with(|m| m.set(None));
            assert_eq!(result.err(), Some(format!("CodexTuiNotAttempted:{reason}")));
            assert!(!std::path::Path::new(&tmux_owner_path("source-mode-held")).exists());
            assert!(
                !std::path::Path::new(&crate::services::tmux_common::session_temp_path(
                    "source-mode-held",
                    "sh"
                ))
                .exists()
            );
            assert!(!root.path().join("runtime/binding_contexts").exists());
        }
        assert_eq!(CodexSourceMode::parse(None).launch_policy(), Ok("legacy"));
        assert_eq!(
            CodexSourceMode::parse(Some("legacy")).launch_policy(),
            Ok("legacy")
        );
        assert_eq!(
            CodexSourceMode::Verified.launch_policy(),
            Err("SourceModeVerifiedNotLanded")
        );
    }

    #[test]
    fn binding_context_t7_codex_launch_fails_before_tmux() {
        use super::prepare_codex_tui_launch_script as launch;
        let options = CodexLaunchOptions::new("");
        crate::services::tui_prompt_dedupe::binding_context::tests::launch_failures(
            |t| {
                launch(
                    t,
                    None,
                    "",
                    &options,
                    (None, None),
                    false,
                    &crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
                        ProviderKind::Codex,
                    ),
                )
                .map(|_| ())
            },
            "sh",
        );
    }

    // A channel configured for Herdr is refused before the owner marker, launch script or tmux.
    #[test]
    fn codex_launch_writes_nothing_for_a_herdr_configured_channel() {
        use crate::services::herdr_launch::HERDR_NOT_ADMITTED;
        use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests};
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, _env) = tests::fixture_after_shared_test_env_lock();
        let _tmux = tests::fake_tmux(root.path());
        let _hosts = crate::config::session_hosts::force_for_test(None, &[(51, "mac-mini")]);
        let tmux = "AgentDesk-codex-herdr-configured";
        let refused = prepare_codex_tui_launch_script(
            tmux,
            None,
            "",
            &CodexLaunchOptions::new(""),
            (Some(51), None),
            false,
            &crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
                ProviderKind::Codex,
            ),
        );
        assert_eq!(refused.err().as_deref(), Some(HERDR_NOT_ADMITTED));
        assert!(!std::path::Path::new(&tmux_owner_path(tmux)).exists());
        let script = crate::services::tmux_common::session_temp_path(tmux, "sh");
        assert!(!std::path::Path::new(&script).exists());
        assert!(!root.path().join("tmux.calls").exists());
    }

    #[test]
    fn codex_tui_hooks_need_the_advertised_bypass_not_trust_hashes() {
        use crate::services::claude_tui::hook_bundle::codex_hook_capability;
        let base = build_codex_tui_args(&CodexLaunchOptions::new(""));
        let overrides = || vec!["hooks.SessionStart=[]".to_string()];
        for version in [Some("codex-cli 0.157.1"), Some("codex-cli 0.130.0"), None] {
            let mut args = base.clone();
            let mut built = false;
            let injected =
                add_codex_tui_hooks(&mut args, codex_hook_capability(version, false), || {
                    built = true;
                    overrides()
                });
            assert!(!injected && !built, "{version:?}");
            assert_eq!(
                args, base,
                "no hook override or trust hash without the bypass"
            );
        }

        let mut args = base.clone();
        assert!(!add_codex_tui_hooks(
            &mut args,
            codex_hook_capability(None, true),
            Vec::new
        ));
        assert_eq!(args, base, "no bypass without hook overrides");

        let mut args = base.clone();
        assert!(add_codex_tui_hooks(
            &mut args,
            codex_hook_capability(Some("codex-cli 0.157.1"), true),
            overrides
        ));
        assert_eq!(args[0], "--dangerously-bypass-hook-trust");
        assert!(args.iter().any(|arg| arg == "hooks.SessionStart=[]"));
    }

    #[test]
    fn codex_launch_home_follows_the_auth_profile_and_hooks_pin_only_an_unprofiled_home() {
        use crate::config::TestEnvVarGuard as Guard;
        use crate::services::provider_auth_profile::ProviderAuthOverlay;
        use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests};
        use std::os::unix::fs::PermissionsExt;
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, _env) = tests::fixture_after_shared_test_env_lock();
        let _tmux = tests::fake_tmux(root.path());
        let _endpoint = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = root.path();
        let codex = dir.join("codex");
        std::fs::write(
            &codex,
            "#!/bin/bash\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.157.1'\n\
             elif [ \"$1 $2\" = 'resume --help' ]; then echo '--dangerously-bypass-hook-trust'\n\
             else printf 'home=%s\\n' \"$CODEX_HOME\"; printf 'arg=%s\\n' \"$@\"; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &codex);
        let _server = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &dir.join("server"));
        let mut baseline = std::collections::HashMap::new();
        for mode in [
            CodexSourceMode::parse(None),
            CodexSourceMode::parse(Some("legacy")),
            CodexSourceMode::Shadow,
        ] {
            SOURCE_MODE_TEST.with(|m| m.set(Some(mode)));
            for (flag, hooks) in [(Some("0"), false), (Some("1"), true), (None, true)] {
                let _flag =
                    Guard::capture_after_shared_test_env_lock("AGENTDESK_CODEX_DIRECT_TUI_HOOKS");
                match flag {
                    Some(value) => unsafe {
                        std::env::set_var("AGENTDESK_CODEX_DIRECT_TUI_HOOKS", value)
                    },
                    None => unsafe { std::env::remove_var("AGENTDESK_CODEX_DIRECT_TUI_HOOKS") },
                }
                let _published = hooks.then(|| {
                    crate::services::claude_tui::hook_server::publish_hook_endpoint(
                        "http://127.0.0.1:9".to_string(),
                    )
                });
                for profile in [false, true] {
                    let mut overlay = ProviderAuthOverlay::default_for(ProviderKind::Codex);
                    if profile {
                        let home = dir.join("profile").display().to_string();
                        overlay.env.insert("CODEX_HOME".into(), home);
                    }
                    let tmux = format!("codex-home-{}-{profile}", flag.unwrap_or("unset"));
                    let options = CodexLaunchOptions::new("actual argv\n한글");
                    let script = prepare_codex_tui_launch_script(
                        &tmux,
                        None,
                        "",
                        &options,
                        (None, None),
                        false,
                        &overlay,
                    )
                    .unwrap();
                    assert_eq!(
                        script.prepared.context.source_policy.as_deref(),
                        mode.launch_policy().ok()
                    );
                    use sha2::{Digest, Sha256};
                    assert_eq!(
                        script.prepared.context.first_prompt_digest,
                        Some(format!(
                            "sha256:{:x}",
                            Sha256::digest(options.prompt.as_bytes())
                        ))
                    );
                    assert_eq!(
                        crate::services::tui_prompt_dedupe::binding_context::input_context(
                            "codex",
                            &script.prepared.context.execution_nonce
                        ),
                        Some(script.prepared.context.clone())
                    );
                    let output = std::process::Command::new("/bin/bash")
                        .arg(&script.script_path)
                        .env("CODEX_HOME", dir.join("tmux"))
                        .output()
                        .unwrap();
                    let stdout = String::from_utf8(output.stdout).unwrap();
                    let expected = match (profile, hooks) {
                        (true, _) => dir.join("profile"),
                        (false, false) => dir.join("tmux"),
                        (false, true) => dir.join("server"),
                    };
                    let case = format!("flag={flag:?} profile={profile}");
                    assert!(
                        stdout.contains("arg=actual argv\n한글\n"),
                        "exact argv bytes: {stdout}"
                    );
                    if let Some(old) = baseline.get(&case) {
                        assert_eq!(&stdout, old, "mode must not change native argv/home/hooks");
                    } else {
                        baseline.insert(case.clone(), stdout.clone());
                    }

                    assert!(
                        stdout.contains(&format!("home={}\n", expected.display())),
                        "child must run under the auth profile home, else the hook-verified one: {case}\n{stdout}"
                    );
                    assert_eq!(
                        stdout.contains("arg=--dangerously-bypass-hook-trust\n"),
                        hooks,
                        "hook argv follows the flag: {case}"
                    );
                    if hooks || profile {
                        assert_eq!(
                            script.prepared.context.provider_root,
                            Some(expected.join("sessions")),
                            "recorded root must be the child's home: {case}"
                        );
                    }
                }
            }
            dedupe::reset_state_for_tests();
        }
        SOURCE_MODE_TEST.with(|m| m.set(None));
    }
}
