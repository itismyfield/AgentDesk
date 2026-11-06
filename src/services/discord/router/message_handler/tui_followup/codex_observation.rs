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
    let runtime_binding = tmux_session_name.and_then(codex_binding);
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
    use crate::services::codex_tui::rollout_tail::observe_rollout_turn_state;
    use crate::services::tui_turn_state::TuiTurnState;
    match select_rollout(
        current_path,
        provider_session_id,
        sessions_dir,
        runtime_binding,
    ) {
        Rollout::Named(path) => observe_rollout_turn_state(&path),
        Rollout::Newest(path) => match observe_rollout_turn_state(&path) {
            state if state.is_busy() => state,
            _ => TuiTurnState::Unknown,
        },
        // No rollout file found for this cwd — treat as idle (session not yet started).
        Rollout::Absent => TuiTurnState::Idle,
        Rollout::Unresolved => TuiTurnState::Unknown,
    }
}

/// The native end of the turn in Codex pane `tmux`, read from the rollout its binding or its
/// session id names in the pane's own cwd; any other rollout, or none, proves no end.
#[cfg(unix)]
pub(in crate::services::discord::router::message_handler) fn codex_native_end(
    tmux: &str,
    provider_session_id: Option<&str>,
) -> crate::services::codex_tui::busy_inject::NativeEnd {
    use crate::services::codex_tui::busy_inject::{NativeEnd, read_native_end};
    if crate::services::tui_prompt_dedupe::codex_verified_input_blocked(tmux) {
        return NativeEnd::Unknown;
    }
    let cwd = crate::services::tmux_diagnostics::tmux_session_pane_cwd(tmux);
    let binding = codex_binding(tmux);
    match select_rollout(cwd.as_deref(), provider_session_id, None, binding.as_ref()) {
        Rollout::Named(path) => read_native_end(&path),
        _ => NativeEnd::Unknown,
    }
}

#[cfg(unix)]
fn codex_binding(tmux: &str) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).filter(|binding| {
        binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
    })
}

/// The rollout a Codex pane's turn is read from.
#[cfg(unix)]
enum Rollout {
    /// The pane's binding names it, or its session id does, in the pane's cwd.
    Named(std::path::PathBuf),
    /// Only the newest rollout of the cwd; nothing ties it to the pane.
    Newest(std::path::PathBuf),
    /// No rollout exists for the cwd.
    Absent,
    /// No cwd, a missing or foreign bound rollout, or a session id that names none.
    Unresolved,
}

#[cfg(unix)]
fn select_rollout(
    current_path: Option<&str>,
    provider_session_id: Option<&str>,
    sessions_dir: Option<&std::path::Path>,
    runtime_binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> Rollout {
    let Some(current_path) = current_path else {
        return Rollout::Unresolved;
    };
    let cwd = std::path::Path::new(current_path);
    if let Some(binding) = runtime_binding {
        let rollout_path = std::path::Path::new(&binding.output_path);
        if std::fs::metadata(rollout_path).is_err() {
            return Rollout::Unresolved;
        }
        if !crate::services::codex_tui::rollout_tail::rollout_file_matches_cwd(rollout_path, cwd) {
            return Rollout::Unresolved;
        }
        return Rollout::Named(rollout_path.to_path_buf());
    }
    let resolved = sessions_dir
        .map(std::path::Path::to_path_buf)
        .or_else(|| crate::services::codex_tui::rollout_tail::default_codex_sessions_dir());
    let Some(sessions_dir) = resolved else {
        return Rollout::Unresolved;
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
        return selection
            .rollout_path
            .map_or(Rollout::Unresolved, Rollout::Named);
    }
    match crate::services::codex_tui::rollout_tail::latest_rollout_for_cwd_since(
        cwd,
        std::time::SystemTime::UNIX_EPOCH,
        &sessions_dir,
    ) {
        Some(rollout_path) => Rollout::Newest(rollout_path),
        None => Rollout::Absent,
    }
}
