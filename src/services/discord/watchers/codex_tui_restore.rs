use crate::services::{
    agent_protocol::RuntimeHandoffKind,
    provider::ProviderKind,
    tmux_common,
    tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding},
};
use poise::serenity_prelude::ChannelId;

#[cfg(unix)]
use crate::services::discord::tui_prompt_relay::rehydration::codex_tui_rehydrated_binding_from_rollout_path;

#[derive(Debug)]
pub(super) struct DirectResumeFallback {
    output_path: String,
    #[cfg(unix)]
    binding: TuiRuntimeBinding,
}

impl DirectResumeFallback {
    pub(super) fn output_path(&self) -> &str {
        &self.output_path
    }
}

/// #2795 — for codex_tui sessions whose AgentDesk-side relay JSONL does not
/// exist on disk, look up the actual codex rollout transcript by the
/// inflight `session_id`. Returns `None` when the inflight is absent, is not
/// a codex_tui handoff, lacks a session_id, or no rollout matches.
pub(super) fn rollout_fallback_for_session(
    provider: &crate::services::provider::ProviderKind,
    channel_id: ChannelId,
) -> Option<String> {
    if *provider != crate::services::provider::ProviderKind::Codex {
        return None;
    }
    let state =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())?;
    if !matches!(
        state.runtime_kind,
        Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
    ) {
        return None;
    }
    let session_id = state.session_id.as_deref()?;
    let rollout = crate::services::codex_tui::rollout_tail::find_rollout_by_session_id(session_id)?;
    Some(rollout.display().to_string())
}

/// #3815 — dcserver restart recovery must also adopt legacy/direct Codex TUI
/// panes that were launched as `codex resume <session-id>` instead of through
/// the current ADK-managed marker path. Those panes can survive deploys with no
/// AgentDesk JSONL/FIFO/marker files, so the normal restore path used to skip
/// them as "no output file" even while tmux was live.
pub(super) fn rollout_fallback_for_live_direct_resume(
    provider: &crate::services::provider::ProviderKind,
    tmux_session_name: &str,
    _channel_id: ChannelId,
) -> Option<DirectResumeFallback> {
    #[cfg(not(unix))]
    {
        let _ = (provider, tmux_session_name);
        return None;
    }

    #[cfg(unix)]
    {
        if *provider != ProviderKind::Codex {
            return None;
        }
        let session_id = codex_resume_session_id_from_tmux_pane(tmux_session_name)?;
        let rollout =
            crate::services::codex_tui::rollout_tail::find_rollout_by_session_id(&session_id)?;
        let binding = codex_tui_rehydrated_binding_from_rollout_path(
            tmux_session_name,
            &rollout,
            Some(session_id),
        )?;
        Some(DirectResumeFallback {
            output_path: rollout.display().to_string(),
            binding,
        })
    }
}

pub(super) fn commit_live_direct_resume_fallback(
    tmux_session_name: &str,
    channel_id: ChannelId,
    fallback: Option<DirectResumeFallback>,
    candidate: Option<TuiRuntimeBinding>,
    watcher_output: &str,
    claim_watcher: impl FnOnce() -> bool,
) -> bool {
    #[cfg(not(unix))]
    {
        let _ = (
            tmux_session_name,
            channel_id,
            fallback,
            candidate,
            watcher_output,
        );
        claim_watcher()
    }

    #[cfg(unix)]
    {
        let Some(fallback) = fallback else {
            return tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
                let unresolved = TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::CodexTui,
                    output_path: String::new(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: None,
                    last_offset: 0,
                    relay_last_offset: None,
                };
                if dedupe::codex_verified_publication_allowed(authority, &unresolved) {
                    return claim_watcher();
                }
                let Some(candidate) = candidate else {
                    return false;
                };
                if candidate.runtime_kind != RuntimeHandoffKind::CodexTui
                    || !dedupe::codex_verified_publication_allowed(authority, &candidate)
                {
                    return false;
                }
                let Some(current) =
                    dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)
                else {
                    return false;
                };
                codex_bindings_same_source(&candidate, &current)
                    && watcher_output_matches(tmux_session_name, watcher_output, &current)
                    && claim_watcher()
            });
        };
        tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
            let Some(current) = rollout_fallback_for_live_direct_resume(
                &ProviderKind::Codex,
                tmux_session_name,
                channel_id,
            ) else {
                return false;
            };
            if !codex_bindings_same_source(&fallback.binding, &current.binding) {
                return false;
            }
            if crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
                .is_some_and(|marker| {
                    !crate::services::codex_tui::session::codex_tui_rollout_paths_same(
                        std::path::Path::new(&current.binding.output_path),
                        &marker.rollout_path,
                    ) || marker.session_id.is_some_and(|session| {
                        current.binding.session_id.as_deref().map(str::trim) != Some(session.trim())
                    })
                })
            {
                return false;
            }
            let binding =
                match dedupe::runtime_binding_for_tmux_session_under_source_authority(authority) {
                    Some(binding) if codex_bindings_same_source(&current.binding, &binding) => {
                        binding
                    }
                    Some(_) => return false,
                    None => current.binding,
                };
            if !dedupe::codex_verified_publication_allowed(authority, &binding)
                || !watcher_output_matches(tmux_session_name, watcher_output, &binding)
            {
                return false;
            }
            let verified = dedupe::codex_verified_marker_metadata(
                authority,
                std::path::Path::new(&binding.output_path),
                binding.session_id.as_deref(),
            )
            .is_ok_and(|metadata| metadata.is_some());
            commit_registration(verified, claim_watcher, || {
                let registered =
                    dedupe::register_rehydrated_tmux_runtime_binding_under_source_authority(
                        authority,
                        ProviderKind::Codex.as_str(),
                        channel_id.get(),
                        binding,
                    );
                if !verified || registered {
                    tmux_common::write_tmux_runtime_kind_marker(
                        tmux_session_name,
                        RuntimeHandoffKind::CodexTui,
                    )
                    .ok();
                }
                registered
            })
        })
    }
}

#[cfg(all(test, unix))]
pub(crate) fn commit_codex_watcher_restore_for_tests(
    tmux_session_name: &str,
    channel_id: ChannelId,
    candidate: Option<TuiRuntimeBinding>,
    watcher_output: &str,
    claim_watcher: impl FnOnce() -> bool,
) -> bool {
    commit_live_direct_resume_fallback(
        tmux_session_name,
        channel_id,
        None,
        candidate,
        watcher_output,
        claim_watcher,
    )
}

#[cfg(unix)]
fn watcher_output_matches(tmux: &str, output: &str, binding: &TuiRuntimeBinding) -> bool {
    let output = std::path::Path::new(output);
    if crate::services::codex_tui::session::codex_tui_rollout_paths_same(
        output,
        std::path::Path::new(&binding.output_path),
    ) {
        return true;
    }
    // Relay output belongs to this tmux namespace, independently of its native rollout.
    let relay = tmux_common::session_temp_path(tmux, "jsonl");
    output == std::path::Path::new(&relay)
        && binding.relay_output_path.as_deref() == Some(relay.as_str())
}

#[cfg(unix)]
fn commit_registration(
    verified: bool,
    claim: impl FnOnce() -> bool,
    register: impl FnOnce() -> bool,
) -> bool {
    if verified {
        // Failed publication must not reserve a watcher that will never start.
        register() && claim()
    } else if claim() {
        register();
        true
    } else {
        false
    }
}

#[cfg(unix)]
fn codex_bindings_same_source(left: &TuiRuntimeBinding, right: &TuiRuntimeBinding) -> bool {
    crate::services::codex_tui::session::codex_tui_rollout_paths_same(
        std::path::Path::new(&left.output_path),
        std::path::Path::new(&right.output_path),
    ) && left.session_id.as_deref().map(str::trim) == right.session_id.as_deref().map(str::trim)
}

fn codex_resume_session_id_from_tmux_pane(tmux_session_name: &str) -> Option<String> {
    let pane_pid = crate::services::platform::tmux::pane_pid(tmux_session_name)?;
    let process_args = crate::services::platform::tmux::read_process_args(pane_pid)?;
    codex_resume_session_id_from_process_args(&process_args)
}

fn codex_resume_session_id_from_process_args(process_args: &str) -> Option<String> {
    let mut saw_codex_binary = false;
    let mut saw_exec_before_resume = false;
    let mut after_resume = false;
    for raw in process_args.split_whitespace() {
        let token = raw.trim_matches(|ch| ch == '\'' || ch == '"' || ch == ',');
        let token_lower = token.to_ascii_lowercase();
        let token_leaf = token_lower
            .rsplit('/')
            .next()
            .unwrap_or(token_lower.as_str());

        if token_leaf.contains("codex-tmux-wrapper") {
            return None;
        }
        if crate::services::cluster::session_matcher::detect_provider_from_pane_command(token)
            == Some(crate::services::provider::ProviderKind::Codex)
        {
            saw_codex_binary = true;
            continue;
        }
        if token == "exec" && !after_resume {
            saw_exec_before_resume = true;
            continue;
        }
        if token == "resume" && saw_codex_binary && !saw_exec_before_resume {
            after_resume = true;
            continue;
        }
        if !after_resume {
            continue;
        }
        if uuid::Uuid::parse_str(token).is_ok() {
            return Some(token.to_string());
        }
    }
    None
}

#[cfg(test)]
mod codex_direct_resume_args_tests {
    #[test]
    fn extracts_resume_session_id_from_direct_codex_pane_args() {
        let args = "/opt/homebrew/bin/node /opt/homebrew/bin/codex resume \
            019e660d-4859-7522-9cee-8ba7c4e7c743 \
            --dangerously-bypass-hook-trust";

        assert_eq!(
            super::codex_resume_session_id_from_process_args(args).as_deref(),
            Some("019e660d-4859-7522-9cee-8ba7c4e7c743")
        );
    }

    #[test]
    fn ignores_wrapper_and_exec_shapes() {
        assert_eq!(
            super::codex_resume_session_id_from_process_args(
                "/usr/local/bin/agentdesk codex-tmux-wrapper resume \
                 019e660d-4859-7522-9cee-8ba7c4e7c743"
            ),
            None
        );
        assert_eq!(
            super::codex_resume_session_id_from_process_args(
                "/opt/homebrew/bin/codex exec resume \
                 019e660d-4859-7522-9cee-8ba7c4e7c743"
            ),
            None
        );
    }
}

#[cfg(test)]
#[cfg(unix)]
mod verified_watcher_claim_tests {
    use super::*;
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };

    #[test]
    fn actual_watcher_without_fallback_refuses_verified_without_proof_and_preserves_legacy() {
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        dedupe::reset_state_for_tests();
        let sessions = root.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let tmux = format!("verified-w-{}", uuid::Uuid::new_v4().simple());
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.clone(),
            channel_id: Some(584_505),
            owner_runtime_root: tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(sessions.canonicalize().unwrap()),
            first_prompt_digest: Some(format!("sha256:{}", "a".repeat(64))),
            source_policy: Some("verified".into()),
        };
        let _prepared = PreparedIncarnation::create(context.clone()).unwrap();
        let nonce = tmux_common::session_temp_path(&tmux, "spawn_nonce");
        std::fs::create_dir_all(std::path::Path::new(&nonce).parent().unwrap()).unwrap();
        std::fs::write(nonce, context.execution_nonce).unwrap();
        dedupe::register_tmux_channel(&tmux, 584_505);
        let claims = std::cell::Cell::new(0);
        assert!(!commit_live_direct_resume_fallback(
            &tmux,
            ChannelId::new(584_505),
            None,
            None,
            "",
            || {
                claims.set(claims.get() + 1);
                true
            },
        ));
        assert_eq!(claims.get(), 0);
        assert!(dedupe::runtime_binding_for_tmux_session(&tmux).is_none());
        assert!(dedupe::provider_session_for_tmux("codex", &tmux).is_none());
        assert!(
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(&tmux).is_none()
        );
        let legacy = format!("legacy-w-{}", uuid::Uuid::new_v4().simple());
        assert!(commit_live_direct_resume_fallback(
            &legacy,
            ChannelId::new(584_505),
            None,
            None,
            "",
            || {
                claims.set(claims.get() + 1);
                true
            },
        ));
        assert_eq!(claims.get(), 1);
        let candidate = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: "/legacy/native.jsonl".into(),
            relay_output_path: Some("/tmp/legacy-relay.jsonl".into()),
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: Some(0),
        };
        assert!(commit_live_direct_resume_fallback(
            &legacy,
            ChannelId::new(584_505),
            None,
            Some(candidate),
            "/tmp/legacy-relay.jsonl",
            || {
                claims.set(claims.get() + 1);
                true
            },
        ));
        assert_eq!(claims.get(), 2);
    }
}

#[cfg(test)]
#[cfg(unix)]
mod verified_commit_order_tests {
    #[test]
    fn actual_commit_helper_publishes_before_claim_and_failed_publish_claims_nothing() {
        for (verified, registered, expected) in [
            (true, true, vec!["register", "claim"]),
            (true, false, vec!["register"]),
            (false, true, vec!["claim", "register"]),
            (false, false, vec!["claim", "register"]),
        ] {
            let calls = std::cell::RefCell::new(Vec::new());
            let result = super::commit_registration(
                verified,
                || {
                    calls.borrow_mut().push("claim");
                    true
                },
                || {
                    calls.borrow_mut().push("register");
                    registered
                },
            );
            assert_eq!(*calls.borrow(), expected);
            assert_eq!(result, !verified || registered);
        }
    }
}
