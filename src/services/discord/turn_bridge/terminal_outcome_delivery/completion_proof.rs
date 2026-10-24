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
