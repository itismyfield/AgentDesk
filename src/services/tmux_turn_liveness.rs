use std::path::Path;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::platform::tmux::{SessionPresence, session_presence};
use crate::services::provider::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndependentTmuxReadiness {
    ReadyForInput,
    Missing,
    LiveOrAmbiguous,
}

/// Probe one local tmux session without treating probe failure or ambiguous
/// provider output as terminal evidence. Structured JSONL state is authoritative;
/// pane scraping is used only when structured state is unavailable.
pub(crate) fn independent_tmux_readiness(
    tmux_session_name: &str,
    provider: &ProviderKind,
    runtime_kind: Option<RuntimeHandoffKind>,
    output_path: Option<&Path>,
    last_offset: Option<u64>,
) -> IndependentTmuxReadiness {
    match session_presence(tmux_session_name) {
        SessionPresence::Missing => return IndependentTmuxReadiness::Missing,
        SessionPresence::ProbeFailed => return IndependentTmuxReadiness::LiveOrAmbiguous,
        SessionPresence::Present => {}
    }

    let structured = output_path.and_then(|path| {
        crate::services::tui_turn_state::jsonl_ready_for_input(
            provider,
            runtime_kind,
            path,
            last_offset,
        )
    });
    let ready = structured.map(crate::services::tui_turn_state::TuiReadyState::is_ready);
    let ready = ready.or_else(|| {
        crate::services::provider::tmux_session_fallback_ready_for_input(
            tmux_session_name,
            provider,
            runtime_kind,
        )
        .map(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
    });

    if ready == Some(true) {
        IndependentTmuxReadiness::ReadyForInput
    } else {
        IndependentTmuxReadiness::LiveOrAmbiguous
    }
}
