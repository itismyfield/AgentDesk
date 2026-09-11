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

    /// #5464 B3 — does this identity fail to name ANY single turn?
    ///
    /// `matches_state` compares four axes, and exactly one of them degenerates:
    /// `user_msg_id == 0` matches every other id-0 row. The repo's answer to
    /// that is NOT "refuse id-0" — #3161 established that an id-0 turn must
    /// still clean up its own row, via the dedicated
    /// `clear_inflight_state_if_matches_zero_owned` path, and `turn_start_offset`
    /// was introduced (see the struct doc above) precisely to disambiguate two
    /// consecutive id-0 TUI-direct turns whose `started_at` collides at
    /// `now_string`'s 1-second resolution.
    ///
    /// So the unnameable shape is the CONJUNCTION `user_msg_id == 0 &&
    /// turn_start_offset.is_none()` — an id-0 row with no disambiguator left.
    /// That is the same conjunction every save_store identity gate already uses
    /// (`identity_gate.rs`, `stream_loop_patch.rs`, `bridge_entry.rs`,
    /// `runtime_stamp.rs`, `heartbeat.rs`). An id-0 row that still carries its
    /// offset is nameable and keeps clearing normally, which is what the
    /// watcher terminal-commit, TUI-direct and stall-exit paths rely on.
    pub(in crate::services::discord) fn is_unnameable(&self) -> bool {
        self.user_msg_id == 0 && self.turn_start_offset.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(user_msg_id: u64, turn_start_offset: Option<u64>) -> InflightTurnIdentity {
        InflightTurnIdentity {
            user_msg_id,
            started_at: "2026-09-11T00:00:00Z".into(),
            tmux_session_name: Some("tui-direct".into()),
            turn_start_offset,
        }
    }

    #[test]
    fn only_an_id_zero_row_without_a_disambiguator_is_unnameable_5464() {
        // The unnameable shape is the conjunction, not id-0 alone.
        assert!(identity(0, None).is_unnameable());
        // An id-0 turn that kept its offset still names itself — #3161's
        // self-cleanup paths depend on this staying false.
        assert!(!identity(0, Some(10)).is_unnameable());
        // A real Discord anchor is always nameable, offset or not.
        assert!(!identity(7, None).is_unnameable());
        assert!(!identity(7, Some(10)).is_unnameable());
    }
}
