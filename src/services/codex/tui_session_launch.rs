//! Tui session launch.

use super::*;

/// Resolve the Codex binary, build the launch args + env, render and write the
/// launch script, and register the Discord-originated prompt for dedupe.
///
/// Returns the resolved binary, script path, owner-marker path, and the
/// rollout "modified since" stamp captured just before the script is written.
/// Errors propagate exactly as the inline body did (`?`).
#[cfg(unix)]
pub(super) fn prepare_codex_tui_launch_script(
    tmux_session_name: &str,
    session_id: Option<&str>,
    prompt: &str,
    launch_options: &CodexLaunchOptions,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    warm_followup_enabled: bool,
    auth_env_lines: &str,
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
    let mut env_lines = build_tmux_launch_env_lines(
        resolution.exec_path.as_deref(),
        report_channel_id,
        report_provider,
    );
    env_lines.push_str(auth_env_lines);
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
    Ok(CodexTuiLaunchScript {
        script_path,
        owner_path,
        rollout_modified_since,
    })
}
