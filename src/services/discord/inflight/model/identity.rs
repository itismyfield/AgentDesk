use super::{Deserialize, InflightTurnState, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct InflightTurnIdentity {
    pub user_msg_id: u64,
    pub started_at: String,
    pub tmux_session_name: Option<String>,
    /// #3041 P1-3 (codex P1-3 issue 2): the turn's `turn_start_offset` — the JSONL
    /// byte offset at which this turn began. Disambiguates two consecutive
    /// `user_msg_id == 0` TUI-direct turns whose `started_at` collides at
    /// `now_string`'s 1-second resolution; monotonic per turn → unique identity.
    pub turn_start_offset: Option<u64>,
}

impl InflightTurnIdentity {
    pub(in crate::services::discord) fn from_state(state: &InflightTurnState) -> Self {
        Self {
            user_msg_id: state.user_msg_id,
            started_at: state.started_at.clone(),
            tmux_session_name: state.tmux_session_name.clone(),
            turn_start_offset: state.turn_start_offset,
        }
    }

    pub(in crate::services::discord) fn matches_state(&self, state: &InflightTurnState) -> bool {
        self.user_msg_id == state.user_msg_id
            && self.started_at == state.started_at
            && self.tmux_session_name == state.tmux_session_name
            // #3419 R3 (codex MEDIUM): keep the clear key == full-struct-eq decision key (TOCTOU on offset-only-diff rows).
            && self.turn_start_offset == state.turn_start_offset
    }
}
