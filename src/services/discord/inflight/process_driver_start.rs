//! Marks a start whose turn runs on a process driver (stream-json/process CLI) rather than a TUI.
//! Headless intake and Discord intake both call this so the resumed-session banner rule matches.
use super::InflightTurnState;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::turn_host::TurnHost;

/// Who started the turn. Headless intake already ran managed providers without a runtime on the
/// process driver; Discord intake leaves those starts out of the suppression scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum StartEntry {
    Headless,
    DiscordIntake,
}

/// Sets `headless_process_start` when the start runs on a process driver: an explicit process
/// backend, or a provider without a managed tmux backend (agy, grok, opencode). TUI/Herdr stay false.
pub(in crate::services::discord) fn stamp_process_driver_start(
    state: &mut InflightTurnState,
    host: &TurnHost,
    entry: StartEntry,
) {
    let process_cli = state
        .provider_kind()
        .is_some_and(|provider| !provider.uses_managed_tmux_backend());
    state.headless_process_start = matches!(host, TurnHost::Tmux)
        && match state.runtime_kind {
            Some(RuntimeHandoffKind::ProcessBackend) => true,
            Some(_) => false,
            None => process_cli || entry == StartEntry::Headless,
        };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    /// The row Discord intake builds just before `turn_host::for_turn` for a resumed turn.
    fn intake_resume_row(
        provider: ProviderKind,
        runtime: Option<RuntimeHandoffKind>,
    ) -> InflightTurnState {
        let tmux = runtime.map(|_| "AgentDesk-test".to_string());
        let mut state = InflightTurnState::new(
            provider,
            4_795_001,
            Some("personal".to_string()),
            1,
            2,
            3,
            "hello".to_string(),
            Some("resume-session-id".to_string()),
            tmux,
            None,
            None,
            0,
        );
        state.runtime_kind = runtime;
        state
    }

    #[test]
    fn intake_agy_resume_turn_marks_process_driver_start() {
        let mut state = intake_resume_row(ProviderKind::Antigravity, None);
        stamp_process_driver_start(&mut state, &TurnHost::Tmux, StartEntry::DiscordIntake);
        assert!(state.headless_process_start);
    }

    #[test]
    fn intake_claude_tui_turn_keeps_banner() {
        let mut state =
            intake_resume_row(ProviderKind::Claude, Some(RuntimeHandoffKind::ClaudeTui));
        stamp_process_driver_start(&mut state, &TurnHost::Tmux, StartEntry::DiscordIntake);
        assert!(!state.headless_process_start);
    }

    #[test]
    fn intake_claude_without_runtime_is_not_widened() {
        let mut state = intake_resume_row(ProviderKind::Claude, None);
        stamp_process_driver_start(&mut state, &TurnHost::Tmux, StartEntry::DiscordIntake);
        assert!(!state.headless_process_start);
    }

    #[test]
    fn managed_provider_on_explicit_process_backend_marks_start() {
        let mut state = intake_resume_row(
            ProviderKind::Claude,
            Some(RuntimeHandoffKind::ProcessBackend),
        );
        stamp_process_driver_start(&mut state, &TurnHost::Tmux, StartEntry::DiscordIntake);
        assert!(state.headless_process_start);
    }

    #[test]
    fn headless_entry_keeps_managed_start_without_runtime() {
        let mut state = intake_resume_row(ProviderKind::Claude, None);
        stamp_process_driver_start(&mut state, &TurnHost::Tmux, StartEntry::Headless);
        assert!(state.headless_process_start);
    }
}
