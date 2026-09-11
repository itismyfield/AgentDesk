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

    /// #5464 B3 — the single "has this turn's dispatch resolved?" judgment.
    ///
    /// `user_msg_id == 0` means dispatch has not anchored this turn to a Discord
    /// user message yet. [`Self::matches_state`] then folds such an identity onto
    /// ANY id-0 row of the channel on its first axis (`0 == 0`), and a caller
    /// that built the expected identity from a row it just read supplies the
    /// remaining three axes from that same read — so the 4-axis guard collapses
    /// into "the bytes I read are still on disk", which proves recency, not
    /// ownership. A mid-turn clear that accepts it deletes the LIVE row and the
    /// terminal that follows is suppressed as `no_inflight_row`.
    ///
    /// Kept as one function because the same judgment is made in four places —
    /// the finalizer's mailbox-release guard, the finalizer's inflight-clear
    /// guard, the identity-clear chokepoint and the after-delivery clear. Two
    /// shapes of one test drift the moment a later fix touches only one of them.
    pub(in crate::services::discord) fn is_resolved(&self) -> bool {
        Self::user_msg_id_is_resolved(self.user_msg_id)
    }

    /// Field-level form of [`Self::is_resolved`], for the guards that hold a
    /// bare turn id (the finalizer's `TurnKey`) instead of a built identity.
    pub(in crate::services::discord) fn user_msg_id_is_resolved(user_msg_id: u64) -> bool {
        user_msg_id != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(user_msg_id: u64) -> InflightTurnIdentity {
        InflightTurnIdentity {
            user_msg_id,
            started_at: "2026-09-11T00:00:00Z".into(),
            tmux_session_name: Some("tui-direct".into()),
            turn_start_offset: Some(10),
        }
    }

    #[test]
    fn unresolved_dispatch_identity_is_never_reported_resolved_5464() {
        // An id-0 turn stays unresolved even when every other axis is populated:
        // the disambiguators order two id-0 turns, they do not anchor either one.
        assert!(!identity(0).is_resolved());
        assert!(!InflightTurnIdentity::user_msg_id_is_resolved(0));
        assert!(identity(7).is_resolved());
        assert!(InflightTurnIdentity::user_msg_id_is_resolved(7));
        // Both shapes are the same judgment; a guard may use either.
        assert_eq!(
            identity(0).is_resolved(),
            InflightTurnIdentity::user_msg_id_is_resolved(0)
        );
    }
}
