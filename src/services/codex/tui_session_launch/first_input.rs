//! The argv-bearing create returns a ticket before rollout discovery and response tailing.
use super::*;

pub(in crate::services::codex) struct Create<'a> {
    pub tmux_session_name: &'a str,
    pub working_dir: &'a str,
    pub prompt: &'a str,
    pub auth_profile: &'a str,
    pub launch: CodexTuiLaunchScript,
    pub selection: &'a crate::services::codex_tui::session::CodexTuiSessionSelection,
    pub end_observation:
        &'a mut crate::services::codex_tui::execution_end::producer::LaunchObservation,
}

pub(in crate::services::codex) struct Submitted {
    pub prepared: PreparedIncarnation,
    pub validated_tail: crate::services::codex_tui::tail_launch::ValidatedTail,
    pub rollout_modified_since: std::time::SystemTime,
    pub handoff: crate::services::tui_prompt_dedupe::HandoffGuard,
}

pub(in crate::services::codex) fn create(input: Create<'_>) -> Result<Submitted, String> {
    let Create {
        tmux_session_name,
        working_dir,
        prompt,
        auth_profile,
        launch,
        selection: session_selection,
        end_observation,
    } = input;
    let CodexTuiLaunchScript {
        prepared,
        script_path,
        owner_path,
        rollout_modified_since,
        handoff,
    } = launch;
    end_observation.prepare(&prepared.context);
    let tmux_result = crate::services::platform::tmux::create_session(
        tmux_session_name,
        Some(working_dir),
        &format!("bash {}", shell_escape(&script_path)),
    )?;

    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        remove_failed_tui_launch_files(&owner_path, &script_path);
        crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
            ProviderKind::Codex.as_str(),
            tmux_session_name,
            prompt,
        );
        return Err(format!("tmux error: {}", stderr));
    }

    crate::services::tmux_common::write_tmux_session_auth_profile(tmux_session_name, auth_profile)?;

    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    // Before the spawn marker: the marker is restart evidence that these checks passed.
    let validated_tail = crate::services::codex_tui::tail_launch::validate_selection(
        &prepared.context,
        session_selection,
    )?;
    prepared.finish_spawn(crate::services::discord::stamp_spawn_markers(
        tmux_session_name,
        Some(&prepared),
    ))?;

    end_observation.spawned(&prepared.context);

    Ok(Submitted {
        prepared,
        validated_tail,
        rollout_modified_since,
        handoff,
    })
}

pub(in crate::services::codex) struct Observe<'a> {
    pub submitted: Submitted,
    pub working_dir: &'a str,
    pub sender: Sender<StreamMessage>,
    pub cancel_token: Option<std::sync::Arc<CancelToken>>,
    pub tmux_session_name: &'a str,
    pub prompt: &'a str,
}

pub(in crate::services::codex) fn observe(input: Observe<'_>) -> Result<(), String> {
    let Observe {
        submitted,
        working_dir,
        sender,
        cancel_token,
        tmux_session_name,
        prompt,
    } = input;
    let Submitted {
        prepared: _prepared,
        validated_tail,
        rollout_modified_since,
        handoff: _handoff,
    } = submitted;
    let cancel_token_for_post_tail = cancel_token.clone();
    let tail_result = crate::services::codex_tui::tail_launch::dispatch(
        validated_tail,
        working_dir,
        rollout_modified_since,
        sender.clone(),
        cancel_token,
        tmux_session_name,
        prompt,
    );
    #[cfg(test)]
    let tail_result = if actual_submit_tests::take_tail_fault() {
        Err("injected Codex transcript tail failure".to_owned())
    } else {
        tail_result
    };
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

/// Publish markers and script after the driver completes its cleanup.
pub(in crate::services::codex) fn publish_codex_tui_launch_script(
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

#[cfg(test)]
mod tests;
