//! Live turn-authority identity used by Claude compact steering.

use super::inflight::{InflightTurnState, TurnSource, load_inflight_state_read_only};
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services) struct ManagedCompactTurnIdentity {
    channel_id: u64,
    user_msg_id: u64,
    started_at: String,
    tmux_session_name: String,
    turn_start_offset: Option<u64>,
}

/// #4667: turn sources eligible for auto-compact identity capture/injection.
/// `Managed` is the AgentDesk-launched path; `ExternalInput`/`ExternalAdopted`
/// are interactive Discord-bound sessions (tty / adopted) that #4652 wrongly
/// gated out. `MonitorTriggered` stays excluded (synthetic auto-turn).
pub(in crate::services) fn compact_eligible_turn_source(source: TurnSource) -> bool {
    matches!(
        source,
        TurnSource::Managed | TurnSource::ExternalInput | TurnSource::ExternalAdopted
    )
}

impl ManagedCompactTurnIdentity {
    pub(in crate::services::discord) fn capture(state: &InflightTurnState) -> Option<Self> {
        let tmux_session_name = state
            .tmux_session_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())?;
        // #4667: auto-compact must reach interactive Discord sessions too.
        // #4652 restricted capture to `Managed` turns, which silently gated
        // out `ExternalInput`/`ExternalAdopted` (interactive tty / adopted)
        // sessions and killed their auto-compact. Relax to those three
        // sources; identity-field matching below still pins injection to the
        // exact live turn.
        compact_eligible_turn_source(state.turn_source).then(|| Self {
            channel_id: state.channel_id,
            user_msg_id: state.user_msg_id,
            started_at: state.started_at.clone(),
            tmux_session_name: tmux_session_name.to_string(),
            turn_start_offset: state.turn_start_offset,
        })
    }

    pub(in crate::services) fn capture_live(
        channel_id: u64,
        tmux_session_name: &str,
    ) -> Option<Self> {
        let state = load_inflight_state_read_only(&ProviderKind::Claude, channel_id)?;
        let identity = Self::capture(&state)?;
        (identity.tmux_session_name == tmux_session_name.trim()).then_some(identity)
    }

    pub(in crate::services) fn channel_id(&self) -> u64 {
        self.channel_id
    }

    pub(in crate::services) fn tmux_session_name(&self) -> &str {
        &self.tmux_session_name
    }

    #[cfg(test)]
    pub(in crate::services) fn test_fixture(channel_id: u64, tmux_session_name: &str) -> Self {
        Self {
            channel_id,
            user_msg_id: 9001,
            started_at: "2026-07-19 00:00:00".to_string(),
            tmux_session_name: tmux_session_name.to_string(),
            turn_start_offset: Some(42),
        }
    }
}

pub(in crate::services) fn live_managed_turn_matches(
    expected: &ManagedCompactTurnIdentity,
) -> bool {
    load_inflight_state_read_only(&ProviderKind::Claude, expected.channel_id)
        .is_some_and(|state| managed_turn_matches_state(expected, &state))
}

fn managed_turn_matches_state(
    expected: &ManagedCompactTurnIdentity,
    state: &InflightTurnState,
) -> bool {
    compact_eligible_turn_source(state.turn_source)
        && state.channel_id == expected.channel_id
        && state.user_msg_id == expected.user_msg_id
        && state.started_at == expected.started_at
        && state.tmux_session_name.as_deref() == Some(expected.tmux_session_name.as_str())
        && state.turn_start_offset == expected.turn_start_offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;

    fn state_with_source(source: TurnSource) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            4667,
            Some("adk-cc".to_string()),
            7,
            9001,
            9002,
            "hi".to_string(),
            Some("session-4667".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            42,
        );
        state.turn_source = source;
        state
    }

    /// #4667 mutation-proof: interactive external turns must be compact-eligible.
    /// Restoring the #4652 `Managed`-only gate makes the `ExternalInput`/
    /// `ExternalAdopted` iterations of this test FAIL — `capture()` returns
    /// `None` and `managed_turn_matches_state()` returns `false` for them.
    #[test]
    fn external_interactive_turns_are_compact_eligible() {
        for source in [
            TurnSource::Managed,
            TurnSource::ExternalInput,
            TurnSource::ExternalAdopted,
        ] {
            let state = state_with_source(source);
            let identity = ManagedCompactTurnIdentity::capture(&state)
                .unwrap_or_else(|| panic!("capture() must return Some for {source:?}"));
            assert!(
                managed_turn_matches_state(&identity, &state),
                "live turn match must hold for {source:?}"
            );
        }
    }

    /// `MonitorTriggered` (synthetic auto-turn) stays outside compact scope.
    #[test]
    fn monitor_triggered_turn_stays_gated() {
        let state = state_with_source(TurnSource::MonitorTriggered);
        assert!(
            ManagedCompactTurnIdentity::capture(&state).is_none(),
            "MonitorTriggered turns must not capture a compact identity"
        );
    }
}
