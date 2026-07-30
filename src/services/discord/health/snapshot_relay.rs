use poise::serenity_prelude::ChannelId;

use crate::services::discord::{self as discord, DeliveryLeaseKey, SharedData};
use discord::outbound::delivery_evidence_store::RelayDeliveryEvidence;
use crate::services::provider::ProviderKind;

use super::super::relay_health::{RelayActiveTurn, RelayHealthSnapshot, RelayStallState};

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RelayThreadProofSnapshot {
    pub(super) parent_channel_id: Option<u64>,
    pub(super) thread_channel_id: Option<u64>,
    pub(super) stale_thread_proof: bool,
}

/// #3631: a rebind-origin inflight row (POST /api/inflight/rebind) is a
/// synthetic origin marker — `turn_id`/`dispatch_id` null, `user_msg_id`/
/// `current_msg_id` 0, `full_response` empty — NOT a real user/agent turn.
/// With no mailbox cancel token there is no live turn, so the channel is idle.
/// The classifier previously fell through to `Foreground`, falsely reporting
/// `active_foreground_stream` and stranding queued messages (they never
/// dispatch because no real turn ever ends to drain the queue). A cancel token
/// present means a real turn HAS since started on the adopted session, so it is
/// genuinely active — only treat it as idle when no cancel token is held.
///
/// Pure seam so the idle decision is unit-testable without constructing a full
/// `InflightTurnState`.
pub(super) fn rebind_origin_inflight_is_idle(
    mailbox_has_cancel_token: bool,
    rebind_origin: bool,
) -> bool {
    rebind_origin && !mailbox_has_cancel_token
}

fn ownerless_external_input_inflight_is_idle(
    inflight: Option<&discord::inflight::InflightTurnState>,
) -> bool {
    inflight.is_some_and(discord::inflight::ownerless_external_input_inflight_is_stale)
}

pub(super) fn relay_active_turn_from_inflight(
    mailbox_has_cancel_token: bool,
    inflight: Option<&discord::inflight::InflightTurnState>,
) -> RelayActiveTurn {
    if !mailbox_has_cancel_token && inflight.is_none() {
        return RelayActiveTurn::None;
    }
    if inflight.is_some_and(|state| {
        rebind_origin_inflight_is_idle(mailbox_has_cancel_token, state.rebind_origin)
    }) {
        return RelayActiveTurn::None;
    }
    if ownerless_external_input_inflight_is_idle(inflight) {
        return RelayActiveTurn::None;
    }
    if inflight.is_some_and(|state| {
        state.long_running_placeholder_active || state.task_notification_kind.is_some()
    }) {
        RelayActiveTurn::ExplicitBackground
    } else {
        RelayActiveTurn::Foreground
    }
}

pub(super) fn last_outbound_activity_ms(
    last_relay_ts_ms: i64,
    inflight: Option<&discord::inflight::InflightTurnState>,
) -> Option<i64> {
    if last_relay_ts_ms > 0 {
        return Some(last_relay_ts_ms);
    }
    let inflight = inflight?;
    let has_discord_write_evidence = inflight.current_msg_len > 0
        || inflight.response_sent_offset > 0
        || inflight.last_watcher_relayed_offset.is_some();
    if !has_discord_write_evidence {
        return None;
    }
    discord::inflight::parse_updated_at_unix(&inflight.updated_at)
        .and_then(|seconds| seconds.checked_mul(1000))
}

pub(super) fn trace_relay_health_classification(
    relay_health: &RelayHealthSnapshot,
    relay_stall_state: RelayStallState,
) {
    if relay_stall_state.should_log_at_debug() {
        tracing::debug!(
            target: "agentdesk::discord::relay_health",
            provider = relay_health.provider.as_str(),
            channel_id = relay_health.channel_id,
            relay_stall_state = relay_stall_state.as_str(),
            queue_depth = relay_health.queue_depth,
            tmux_alive = ?relay_health.tmux_alive,
            desynced = relay_health.desynced,
            pending_thread_proof = relay_health.pending_thread_proof,
            "relay health classified"
        );
    } else {
        tracing::trace!(
            target: "agentdesk::discord::relay_health",
            provider = relay_health.provider.as_str(),
            channel_id = relay_health.channel_id,
            relay_stall_state = relay_stall_state.as_str(),
            queue_depth = relay_health.queue_depth,
            "relay health classified"
        );
    }
}

pub(super) async fn relay_thread_proof_for_channel(
    shared: &SharedData,
    provider: Option<&ProviderKind>,
    channel_id: ChannelId,
    current_channel_has_live_evidence: bool,
) -> RelayThreadProofSnapshot {
    let thread_channel_id = shared
        .dispatch
        .thread_parents
        .get(&channel_id)
        .map(|entry| entry.value().get());
    let parent_channel_id = shared
        .dispatch
        .thread_parents
        .iter()
        .find_map(|entry| (*entry.value() == channel_id).then_some(entry.key().get()));
    let child_has_live_evidence = match thread_channel_id {
        Some(thread_id) => {
            let thread_channel = ChannelId::new(thread_id);
            let thread_mailbox = discord::mailbox_snapshot(shared, thread_channel).await;
            let thread_inflight = provider
                .and_then(|provider| discord::inflight::load_inflight_state(provider, thread_id));
            thread_mailbox.cancel_token.is_some()
                || thread_inflight.is_some()
                || shared.tmux_watchers.contains_key(&thread_channel)
        }
        None => false,
    };
    RelayThreadProofSnapshot {
        parent_channel_id,
        thread_channel_id,
        stale_thread_proof: thread_channel_id.is_some_and(|_| !child_has_live_evidence)
            || parent_channel_id.is_some_and(|_| !current_channel_has_live_evidence),
    }
}

pub(super) struct RelayHealthBuildInput {
    pub(super) provider: String,
    pub(super) channel_id: u64,
    pub(super) mailbox_has_cancel_token: bool,
    pub(super) mailbox_active_user_msg_id: Option<u64>,
    pub(super) mailbox_turn_started_at_ms: Option<i64>,
    pub(super) relay_turn_key: Option<DeliveryLeaseKey>,
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
    pub(super) last_relay_offset_recorded: bool,
    pub(super) last_capture_offset: Option<u64>,
    pub(super) unread_bytes: Option<u64>,
    pub(super) desynced: bool,
    pub(super) thread_proof: RelayThreadProofSnapshot,
    pub(super) active_turn: RelayActiveTurn,
    pub(super) last_outbound_activity_ms: Option<i64>,
}

fn relay_delivery_evidence(key: Option<&DeliveryLeaseKey>) -> RelayDeliveryEvidence {
    let Some(key) = key else {
        // NoLease recovery/standby transports and health rows without an exact
        // lease identity are outside this process-local evidence boundary.
        return RelayDeliveryEvidence::Unknown;
    };
    discord::outbound::delivery_evidence_store::relay_evidence_for_turn(key)
}

pub(super) fn relay_turn_key_for_health(
    channel_id: ChannelId,
    generation: u64,
    inflight: Option<&discord::inflight::InflightTurnState>,
) -> Option<DeliveryLeaseKey> {
    let state = inflight?;
    // TUI-direct/external-input delivery has transport paths that never acquire a
    // terminal lease, and its placeholder makes generic outbound activity look
    // live from turn start. Treat the entire class as unobservable here rather
    // than letting a partial lease observation arm destructive recovery.
    if matches!(
        state.turn_source,
        discord::inflight::TurnSource::ExternalInput
            | discord::inflight::TurnSource::ExternalAdopted
    ) {
        return None;
    }
    Some(DeliveryLeaseKey::from_inflight_state_for_site(
        channel_id,
        generation,
        state,
        "relay_health",
    ))
}

pub(super) fn build_relay_health_snapshot(input: RelayHealthBuildInput) -> RelayHealthSnapshot {
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
        queue_depth: input.queue_depth,
        pending_discord_callback_msg_id: input
            .bridge_current_msg_id
            .or(input.mailbox_active_user_msg_id),
        pending_thread_proof: input.thread_proof.parent_channel_id.is_some()
            || input.thread_proof.thread_channel_id.is_some(),
        parent_channel_id: input.thread_proof.parent_channel_id,
        thread_channel_id: input.thread_proof.thread_channel_id,
        last_relay_ts_ms: (input.last_relay_ts_ms > 0).then_some(input.last_relay_ts_ms),
        last_outbound_activity_ms: input.last_outbound_activity_ms,
        delivery_evidence: relay_delivery_evidence(input.relay_turn_key.as_ref()),
        last_capture_offset: input.last_capture_offset,
        last_relay_offset: input.last_relay_offset,
        last_relay_offset_recorded: input.last_relay_offset_recorded,
        unread_bytes: input.unread_bytes,
        desynced: input.desynced,
        stale_thread_proof: input.thread_proof.stale_thread_proof,
    }
}
