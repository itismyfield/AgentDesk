use super::*;

#[cfg(unix)]
pub(in crate::services::discord::router::message_handler) fn observe_codex_tui_rollout_state_for_cwd(
    current_path: Option<&str>,
    tmux_session_name: Option<&str>,
    provider_session_id: Option<&str>,
) -> crate::services::tui_turn_state::TuiTurnState {
    if tmux_session_name
        .is_some_and(crate::services::tui_prompt_dedupe::codex_verified_input_blocked)
    {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    }
    let runtime_binding = tmux_session_name
        .and_then(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session)
        .filter(|binding| {
            binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
        });
    observe_codex_tui_rollout_state_for_cwd_with_sessions(
        current_path,
        provider_session_id,
        None,
        runtime_binding.as_ref(),
    )
}

#[cfg(unix)]
pub(in crate::services::discord::router::message_handler) fn observe_codex_tui_rollout_state_for_cwd_with_sessions(
    current_path: Option<&str>,
    provider_session_id: Option<&str>,
    sessions_dir: Option<&std::path::Path>,
    runtime_binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> crate::services::tui_turn_state::TuiTurnState {
    let Some(current_path) = current_path else {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    };
    let cwd = std::path::Path::new(current_path);
    if let Some(binding) = runtime_binding {
        let rollout_path = std::path::Path::new(&binding.output_path);
        if std::fs::metadata(rollout_path).is_err() {
            return crate::services::tui_turn_state::TuiTurnState::Unknown;
        }
        if !crate::services::codex_tui::rollout_tail::rollout_file_matches_cwd(rollout_path, cwd) {
            return crate::services::tui_turn_state::TuiTurnState::Unknown;
        }
        return crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(rollout_path);
    }
    let resolved = sessions_dir
        .map(std::path::Path::to_path_buf)
        .or_else(|| crate::services::codex_tui::rollout_tail::default_codex_sessions_dir());
    let Some(sessions_dir) = resolved else {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    };
    if let Some(provider_session_id) = provider_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let selection = crate::services::codex_tui::session::resolve_codex_tui_session(
            Some(provider_session_id),
            cwd,
            Some(&sessions_dir),
            false,
        );
        if let Some(rollout_path) = selection.rollout_path.as_deref() {
            return crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(
                rollout_path,
            );
        }
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    }
    let Some(rollout_path) = crate::services::codex_tui::rollout_tail::latest_rollout_for_cwd_since(
        cwd,
        std::time::SystemTime::UNIX_EPOCH,
        &sessions_dir,
    ) else {
        // No rollout file found for this cwd — treat as idle (session not yet started).
        return crate::services::tui_turn_state::TuiTurnState::Idle;
    };
    let rollout_state =
        crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(&rollout_path);
    if rollout_state.is_busy() {
        return rollout_state;
    }
    crate::services::tui_turn_state::TuiTurnState::Unknown
}
