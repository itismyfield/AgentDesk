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

impl ManagedCompactTurnIdentity {
    pub(in crate::services::discord) fn capture(state: &InflightTurnState) -> Option<Self> {
        let tmux_session_name = state
            .tmux_session_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())?;
        (state.turn_source == TurnSource::Managed).then(|| Self {
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
    state.turn_source == TurnSource::Managed
        && state.channel_id == expected.channel_id
        && state.user_msg_id == expected.user_msg_id
        && state.started_at == expected.started_at
        && state.tmux_session_name.as_deref() == Some(expected.tmux_session_name.as_str())
        && state.turn_start_offset == expected.turn_start_offset
}
