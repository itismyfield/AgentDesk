use super::*;
use crate::services::discord::codex_absence::InflightObs;

pub(crate) struct CommandStreamingRequest<'a> {
    pub prompt: &'a str,
    pub session_id: Option<&'a str>,
    pub working_dir: &'a str,
    pub sender: Sender<StreamMessage>,
    pub system_prompt: Option<&'a str>,
    pub allowed_tools: Option<&'a [String]>,
    pub cancel_token: Option<std::sync::Arc<CancelToken>>,
    /// Always `None` in production; clear-ticket tests use its refusal as a launch-free dispatch.
    pub remote_profile: Option<&'a RemoteProfile>,
    pub tmux_session_name: Option<&'a str>,
    pub teardown: Option<&'a TeardownClearance>,
    pub report_channel_id: Option<u64>,
    pub report_provider: Option<ProviderKind>,
    pub model: Option<&'a str>,
    pub fast_mode_enabled: Option<bool>,
    pub goals_enabled: Option<bool>,
    pub compact_token_limit: Option<u64>,
    pub force_fresh_provider_session: bool,
}

pub(crate) fn execute_command_streaming_observed(
    request: CommandStreamingRequest<'_>,
    observation: Option<&InflightObs>,
) -> Result<(), String> {
    let CommandStreamingRequest {
        prompt,
        session_id,
        working_dir,
        sender,
        system_prompt,
        allowed_tools,
        cancel_token,
        remote_profile,
        tmux_session_name,
        teardown,
        report_channel_id,
        report_provider,
        model,
        fast_mode_enabled,
        goals_enabled,
        compact_token_limit,
        force_fresh_provider_session,
    } = request;
    #[cfg(unix)]
    let _end_attempt = crate::services::codex_tui::execution_end::attempt::begin(
        report_channel_id.filter(|_| {
            force_fresh_provider_session
                || !crate::services::codex_tui::execution_end::mutant("fresh_only_take")
        }),
        tmux_session_name,
    );
    #[cfg(unix)]
    let entrypoint_supports_tui_hosting =
        remote_profile.is_none() && tmux_session_name.is_some() && claude::is_tmux_available();
    #[cfg(not(unix))]
    let entrypoint_supports_tui_hosting = false;
    #[cfg(not(unix))]
    let _ = observation;
    let session_selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            &ProviderKind::Codex,
            entrypoint_supports_tui_hosting,
            report_channel_id,
        );
    session_selection.log_start("codex.execute_command_streaming");

    let readonly_mode = is_readonly_tool_policy(allowed_tools);
    let developer_instructions = compose_codex_developer_instructions(system_prompt, allowed_tools);
    let prompt = prompt.to_owned();

    if let Some(profile) = remote_profile {
        // Remote tmux can outlive its SSH owner, so this dispatch path remains refused.
        #[cfg(unix)]
        {
            let requested_remote_tmux = tmux_session_name.is_some()
                && crate::utils::env_flag("AGENTDESK_CODEX_REMOTE_TMUX", false);
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
        // AgentDesk has no remote SSH runtime; no setting opens this path.
        tracing::warn!(
            provider = "codex",
            profile = %profile.name,
            doc = "docs/codex-remote-ssh-policy.md",
            "refusing Codex remote SSH dispatch: remote SSH execution is not available (#2193)"
        );
        return Err("Remote SSH execution is disabled by policy (#2193). \
             See docs/codex-remote-ssh-policy.md."
            .to_string());
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
                if observation.is_none() {
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
                        teardown,
                        report_channel_id,
                        report_provider,
                        developer_instructions.as_deref(),
                        readonly_mode,
                        compact_token_limit,
                        force_fresh_provider_session,
                    );
                }
                return absence_launch::execute_observed(
                    absence_launch::LocalTuiTurn {
                        prompt: &prompt,
                        session_id,
                        model,
                        fast_mode_enabled,
                        goals_enabled,
                        working_dir,
                        sender,
                        cancel_token,
                        tmux_session_name: tmux_name,
                        teardown,
                        report_channel_id,
                        report_provider,
                        developer_instructions: developer_instructions.as_deref(),
                        readonly_mode,
                        compact_token_limit,
                        force_fresh_provider_session,
                    },
                    observation,
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
                teardown,
                report_channel_id,
                report_provider,
                developer_instructions.as_deref(),
                compact_token_limit,
                force_fresh_provider_session,
            );
        }
        #[cfg(not(unix))]
        let _ = teardown;
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
