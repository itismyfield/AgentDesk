//! Rewinds a Herdr start attempt that ended before its first key: its durable row leaves with it,
//! so the same front's next start is not refused by its own earlier attempt.
use super::StreamingTurn;
use crate::services::discord::inflight::{
    self, GuardedClearOutcome, InflightTurnIdentity, InflightTurnState,
};

/// Removes the row of exactly this attempt once its token proves no key reached the pane. The
/// Herdr own-end hold does not apply: a turn that never began has no terminal to wait for.
pub(super) fn unwind(turn: &StreamingTurn<'_>, anchor: u64) {
    let token = &turn.cancel;
    let Some(nonce) = token.turn_nonce().filter(|nonce| !nonce.is_empty()) else {
        return;
    };
    if !crate::services::provider::herdr_before_start::seal_untouched_exit(token) {
        return;
    }
    let generation = crate::services::discord::runtime_store::process_generation();
    let own = |row: &InflightTurnState| {
        row.turn_nonce.as_deref() == Some(nonce)
            && row.user_msg_id == anchor
            && row.born_generation == generation
    };
    // A save between the read and the locked clear moves the identity; a second read settles it.
    for _ in 0..2 {
        let read = inflight::load_inflight_state_read_only(turn.provider, turn.channel_id);
        let Some(row) = read.filter(own) else {
            return;
        };
        let outcome = inflight::clear_inflight_state_for_captured_episode(
            turn.provider,
            turn.channel_id,
            &InflightTurnIdentity::from_state(&row),
            Some(nonce),
        );
        tracing::info!(
            channel_id = turn.channel_id,
            turn_nonce = nonce,
            ?outcome,
            "herdr start held before its first key: own row rewind"
        );
        if outcome != GuardedClearOutcome::UserMsgMismatch {
            return;
        }
    }
}
