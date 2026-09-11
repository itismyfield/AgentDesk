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
    /// `matches_state` compares four axes and NOT ONE of them names a turn by
    /// itself. `user_msg_id` is a constant `0` for every TUI-direct and
    /// watcher-direct turn; `started_at` is `now_string`, formatted to 1-second
    /// resolution, so it collides for turns starting in the same second;
    /// `tmux_session_name` is the pane, shared by consecutive turns of that
    /// pane and `None == None` when absent. Only `turn_start_offset` is
    /// monotonic per turn (struct doc above), so it is the single axis that can
    /// break a tie between two id-0 turns of one channel.
    ///
    /// The unnameable shape is therefore the CONJUNCTION `user_msg_id == 0 &&
    /// turn_start_offset.is_none()` — an id-0 row whose one disambiguator is
    /// gone.
    ///
    /// This predicate deliberately does not read `started_at` or
    /// `tmux_session_name`, and omits nothing by doing so: it is applied in
    /// conjunction with `matches_state` at the clear chokepoint, so on any
    /// input where either of those two axes DIFFERS the four-axis compare
    /// already refuses with the same `UserMsgMismatch`. The predicate can only
    /// change the verdict where both axes already agree — exactly the inputs
    /// where they disambiguate nothing. `clear_store::identity`'s
    /// `unnameable_guard_only_decides_where_started_at_and_tmux_agree_5464`
    /// asserts that decomposition per axis.
    ///
    /// It is not a blanket id-0 refusal: an id-0 turn must still clean up its
    /// OWN row through `clear_inflight_state_if_matches_zero_owned`. That rule
    /// comes from the codex P1 review comment on #3161 as recorded in
    /// `clear_store/mod.rs`'s doc, not from the #3161 issue body (which is a
    /// status-panel edit gate). An id-0 row that still carries its offset stays
    /// nameable, which the watcher terminal-commit, TUI-direct and stall-exit
    /// paths rely on. The same conjunction guards 10 save_store identity sites
    /// across 6 files (`identity_gate.rs` x3, `stream_loop_patch.rs` x3,
    /// `bridge_entry.rs`, `claude_e_stamp.rs`, `heartbeat.rs`,
    /// `runtime_stamp.rs`).
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
