use super::*;

pub(super) fn input_confirmed(
    provider: &ProviderKind,
    state: &InflightTurnState,
    answer: &str,
) -> bool {
    if *provider != ProviderKind::Claude
        || state.runtime_kind
            != Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
    {
        return true;
    }
    if state.has_native_herdr_input() {
        return true;
    }
    if answer.trim().is_empty() {
        return false;
    }
    if external_native_input_confirmed(state) {
        return true;
    }
    state
        .managed_submission_capability()
        .is_some_and(|attempt| attempt.matches_attempt(state) && attempt.exact_user_receipt())
}

pub(super) fn waiting(
    state: &InflightTurnState,
) -> Option<crate::services::claude_tui::input::PreparationHold> {
    if state.provider_kind() != Some(ProviderKind::Claude)
        || state.runtime_kind
            != Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
    {
        return None;
    }
    let attempt = state.managed_submission_capability()?;
    if !attempt.matches_attempt(state) {
        return None;
    }
    attempt
        .preparation_hold()
        .filter(|hold| state.tmux_session_name.as_deref() == Some(hold.target.as_str()))
}

fn external_native_input_confirmed(state: &InflightTurnState) -> bool {
    #[cfg(unix)]
    {
        use crate::services::discord::inflight::TurnSource;
        let (Some(session), Some(start), Some(source), Some(generation)) = (
            state.tmux_session_name.as_deref(),
            state.turn_start_offset,
            state.tui_terminal_source_file_identity,
            state.tui_terminal_generation_mtime_ns,
        ) else {
            return false;
        };
        if state.turn_source != TurnSource::ExternalInput
            || generation <= 0
            || crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(
                session,
            ) != generation
        {
            return false;
        }
        let confirmed = crate::services::claude_tui::input::UserReceipt::from_native_range(
            session,
            &state.user_text,
            state.session_id.as_deref(),
            (start, state.last_offset),
            source,
        )
        .is_some_and(|mut receipt| receipt.confirmed());
        confirmed
            && crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(
                session,
            ) == generation
    }
    #[cfg(not(unix))]
    {
        let _ = state;
        false
    }
}
