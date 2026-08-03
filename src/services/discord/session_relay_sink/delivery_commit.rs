use super::{SessionBoundDiscordRelaySink, SessionRelayDelivery};
use crate::services::provider::ProviderKind;
use serenity::model::id::ChannelId;

impl SessionBoundDiscordRelaySink {
    pub(super) fn advance_offset_for_confirmed_delegated_terminal(
        &self,
        shared: &crate::services::discord::SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        session_name: &str,
        delivery: &SessionRelayDelivery,
        inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
    ) -> bool {
        let Some(end) = delivery.terminal_consumed_end.filter(|end| *end > 0) else {
            return false;
        };
        // IDENTITY GATE: the frame's pinned turn identity must still match the
        // channel's current inflight. A delayed frame from an already-replaced
        // turn (or a cleared inflight) is ignored — never advances a wrong turn.
        let Some(inflight) = inflight else {
            tracing::debug!(
                provider = provider.as_str(),
                channel_id,
                tmux_session = %session_name,
                frame_user_msg_id = delivery.frame_turn_user_msg_id,
                "session-bound sink: terminal frame carried a commit fence but inflight is gone; identity gate blocks advance"
            );
            return false;
        };
        // #3041 P1-3 (codex P1-3 issue 2 R4): STRICT `turn_start_offset` identity — a
        // REQUIRED gate part with NO None fallback (two `user_msg_id == 0` turns in the same
        // second collide on the weak `(user_msg_id, started_at)` pair). A fenced frame is
        // GUARANTEED a real offset by the producer, so `None`/mismatch is a stale/wrong-turn
        // frame → MUST NOT advance (the watcher's SendFull delivers — no black-hole).
        let identity_matches = inflight.user_msg_id == delivery.frame_turn_user_msg_id
            && inflight.started_at == delivery.frame_turn_started_at
            && delivery.frame_turn_start_offset.is_some()
            && inflight.turn_start_offset == delivery.frame_turn_start_offset;
        if !identity_matches {
            tracing::debug!(
                provider = provider.as_str(),
                channel_id,
                tmux_session = %session_name,
                frame_user_msg_id = delivery.frame_turn_user_msg_id,
                inflight_user_msg_id = inflight.user_msg_id,
                frame_turn_start_offset = delivery.frame_turn_start_offset,
                inflight_turn_start_offset = inflight.turn_start_offset,
                "session-bound sink: terminal frame identity != current inflight; identity gate blocks advance (delayed/wrong-turn frame)"
            );
            return false;
        }
        crate::services::discord::tmux::advance_watcher_confirmed_end(
            shared,
            provider,
            ChannelId::new(channel_id),
            session_name,
            end,
            "src/services/discord/session_relay_sink.rs:sink_confirmed_terminal_advance",
        );
        // #3976: stamp the durable per-row delivered marker ONLY here — past the
        // identity gate, after the `confirmed_end_offset` watermark advance fired
        // (so a refused/identity-mismatched advance, which returned above, never
        // marks the row). The watermark is resettable and writes nothing else to
        // the row, so without this durable marker a delivered-but-unmirrored row is
        // indistinguishable from a never-delivered black-hole and orphan-reclaim
        // would re-emit its tail on a watermark reset. The flock RMW re-gates the
        // identity under the lock, so a turn replaced during the POST is never
        // marked. Best-effort: a residual crash between the POST and this write
        // reverts the row to orphan shape on reboot (same at-most-once residual the
        // #3918 marker bounds) — acceptable and no worse than today.
        crate::services::discord::inflight::mark_session_bound_relay_delivered_locked(
            provider,
            channel_id,
            &crate::services::discord::inflight::InflightTurnIdentity::from_state(inflight),
            session_name,
        );
        true
    }
}
