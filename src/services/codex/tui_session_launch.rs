//! Tui session launch.

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;

/// Prepare durable launch evidence and the Codex Direct TUI launch script.
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

    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");
    let prepared = match PreparedIncarnation::prepare(
        "codex",
        tmux_session_name,
        report_channel_id,
        launch_options.resume_session_id.as_deref(),
        launch_options.resume_session_id.is_some(),
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = std::fs::remove_file(&owner_path);
            let _ = std::fs::remove_file(&script_path);
            return Err(error);
        }
    };
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
    env_lines.push_str(auth_env_lines);
    env_lines.push_str(&prepared.env_lines());
    let mut args = build_codex_tui_args(launch_options);
    let hooks_injected = codex_direct_tui_hook_overrides_enabled()
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
    if !hooks_injected {
        tracing::info!(
            tmux_session_name,
            "Codex direct TUI session hook overrides not injected; using rollout transcript tail for relay"
        );
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
        prepared,
        script_path,
        owner_path,
        rollout_modified_since,
    })
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn binding_context_t7_codex_launch_fails_before_tmux() {
        use super::prepare_codex_tui_launch_script as launch;
        let options = CodexLaunchOptions::new("");
        crate::services::tui_prompt_dedupe::binding_context::tests::launch_failures(
            |t| launch(t, None, "", &options, None, None, false, "").map(|_| ()),
            "sh",
        );
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
}
