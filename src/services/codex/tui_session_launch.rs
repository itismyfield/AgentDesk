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
    auth_overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
) -> Result<CodexTuiLaunchScript, String> {
    use crate::services::herdr_launch::{HERDR_NOT_ADMITTED, herdr_configured_for_tui_launch};
    // Refused before any launch I/O; a Herdr channel never gets a tmux session.
    if herdr_configured_for_tui_launch(report_channel_id) {
        return Err(HERDR_NOT_ADMITTED.to_string());
    }
    let policy =
        crate::services::codex_tui::canary::launch_policy(tmux_session_name, report_channel_id)?;
    use sha2::{Digest, Sha256};
    let digest = format!(
        "sha256:{:x}",
        Sha256::digest(launch_options.prompt.as_bytes())
    );
    write_tmux_owner_marker(tmux_session_name)?;
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux_session_name,
        crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
    )?;
    let owner_path = tmux_owner_path(tmux_session_name);

    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");
    // The auth profile decides the child's home; an unpinned one is exported only with hooks.
    let pinned_home = auth_overlay.env.get("CODEX_HOME").map(PathBuf::from);
    let unpinned = pinned_home.is_none() && !auth_overlay.unset.contains("CODEX_HOME");
    let codex_home = match pinned_home {
        Some(home) => Some(home),
        None if unpinned => crate::services::codex_tui::rollout_tail::default_codex_home(),
        None => dirs::home_dir().map(|home| home.join(".codex")),
    };
    let prepared = match PreparedIncarnation::prepare_pinned(
        "codex",
        tmux_session_name,
        report_channel_id,
        launch_options.resume_session_id.as_deref(),
        launch_options.resume_session_id.is_some(),
        codex_home.as_ref().map(|home| home.join("sessions")),
        (Some(digest), Some(policy.to_owned())),
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
    env_lines
        .push_str(&crate::services::provider_auth_profile::overlay_shell_env_lines(auth_overlay));
    env_lines.push_str(&prepared.env_lines());
    env_lines.push_str(&format!(
        "export AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE={}\n",
        shell_escape(policy)
    ));
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
    } else if unpinned && let Some(home) = &codex_home {
        // Hook sources are verified under the recorded root, so the child must not inherit another.
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
        let _state = dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
        let _tmux = binding_context::tests::fake_tmux(root.path());
        let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
        let binary = root.path().join("codex");
        std::fs::write(&binary, "#!/bin/bash\necho 'codex-cli 0.157.1'\n").unwrap();
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
                let script = prepare_codex_tui_launch_script(
                    tmux,
                    None,
                    "actual first prompt",
                    &CodexLaunchOptions::new("actual first prompt"),
                    channel,
                    None,
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
                None,
                None,
                false,
                &crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
                    ProviderKind::Codex,
                ),
            );
            SOURCE_MODE_TEST.with(|m| m.set(None));
            assert_eq!(result.err().as_deref(), Some(reason));
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
                    None,
                    None,
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
            Some(51),
            None,
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
                        &tmux, None, "", &options, None, None, false, &overlay,
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
