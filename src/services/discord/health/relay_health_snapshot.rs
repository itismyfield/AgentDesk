use super::snapshot::RelayThreadProofSnapshot;
use crate::services::discord::relay_health::{
    RelayActiveTurn, RelayHealthSnapshot, observation_age_secs,
};

pub(super) struct RelayHealthBuildInput {
    pub(super) provider: String,
    pub(super) channel_id: u64,
    pub(super) mailbox_has_cancel_token: bool,
    pub(super) mailbox_active_user_msg_id: Option<u64>,
    pub(super) mailbox_turn_started_at_ms: Option<i64>,
    pub(super) unpaired_active_token_reconfirmed: bool,
    pub(super) queue_depth: usize,
    pub(super) watcher_attached: bool,
    pub(super) watcher_attached_stale: bool,
    pub(super) watcher_owner_channel_id: Option<u64>,
    pub(super) tmux_session: Option<String>,
    pub(super) tmux_alive: Option<bool>,
    pub(super) bridge_inflight_present: bool,
    pub(super) bridge_current_msg_id: Option<u64>,
    pub(super) watcher_owns_live_relay: bool,
    pub(super) last_relay_ts_ms: i64,
    pub(super) last_relay_offset: u64,
    pub(super) last_capture_offset: Option<u64>,
    pub(super) unread_bytes: Option<u64>,
    pub(super) desynced: bool,
    pub(super) thread_proof: RelayThreadProofSnapshot,
    pub(super) active_turn: RelayActiveTurn,
    pub(super) last_outbound_activity_ms: Option<i64>,
}

pub(super) fn build_relay_health_snapshot(input: RelayHealthBuildInput) -> RelayHealthSnapshot {
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    let last_relay_ts_ms = (input.last_relay_ts_ms > 0).then_some(input.last_relay_ts_ms);
    RelayHealthSnapshot {
        provider: input.provider,
        channel_id: input.channel_id,
        active_turn: input.active_turn,
        tmux_session: input.tmux_session,
        tmux_alive: input.tmux_alive,
        watcher_attached: input.watcher_attached,
        watcher_attached_stale: input.watcher_attached_stale,
        watcher_owner_channel_id: input.watcher_owner_channel_id,
        watcher_owns_live_relay: input.watcher_owns_live_relay,
        bridge_inflight_present: input.bridge_inflight_present,
        bridge_current_msg_id: input.bridge_current_msg_id,
        mailbox_has_cancel_token: input.mailbox_has_cancel_token,
        mailbox_active_user_msg_id: input.mailbox_active_user_msg_id,
        mailbox_turn_started_at_ms: input.mailbox_turn_started_at_ms,
        mailbox_turn_age_secs: observation_age_secs(
            observed_at_ms,
            input.mailbox_turn_started_at_ms,
        ),
        queue_depth: input.queue_depth,
        pending_discord_callback_msg_id: input
            .bridge_current_msg_id
            .or(input.mailbox_active_user_msg_id),
        pending_thread_proof: input.thread_proof.parent_channel_id.is_some()
            || input.thread_proof.thread_channel_id.is_some(),
        parent_channel_id: input.thread_proof.parent_channel_id,
        thread_channel_id: input.thread_proof.thread_channel_id,
        last_relay_ts_ms,
        last_relay_age_secs: observation_age_secs(observed_at_ms, last_relay_ts_ms),
        last_outbound_activity_ms: input.last_outbound_activity_ms,
        last_capture_offset: input.last_capture_offset,
        last_relay_offset: input.last_relay_offset,
        unread_bytes: input.unread_bytes,
        desynced: input.desynced,
        stale_thread_proof: input.thread_proof.stale_thread_proof,
        unpaired_active_token_reconfirmed: input.unpaired_active_token_reconfirmed,
    }
}
