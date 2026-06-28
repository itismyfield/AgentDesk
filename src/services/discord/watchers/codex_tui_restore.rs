use poise::serenity_prelude::ChannelId;

#[derive(Debug)]
pub(super) struct DirectResumeFallback {
    output_path: String,
    #[cfg(unix)]
    binding: crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
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
        if *provider != crate::services::provider::ProviderKind::Codex {
            return None;
        }
        let session_id = codex_resume_session_id_from_tmux_pane(tmux_session_name)?;
        let rollout =
            crate::services::codex_tui::rollout_tail::find_rollout_by_session_id(&session_id)?;
        let binding = crate::services::discord::tui_prompt_relay::rehydration::codex_tui_rehydrated_binding_from_rollout_path(
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
    fallback: DirectResumeFallback,
) {
    #[cfg(not(unix))]
    {
        let _ = (tmux_session_name, channel_id, fallback);
    }

    #[cfg(unix)]
    {
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            tmux_session_name,
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        )
        .ok();
        crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
            crate::services::provider::ProviderKind::Codex.as_str(),
            tmux_session_name,
            channel_id.get(),
            fallback.binding,
        );
    }
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
