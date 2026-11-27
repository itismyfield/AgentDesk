use crate::services::discord::inflight::InflightTurnState;
#[cfg(test)]
use crate::services::discord::turn_finalizer::TurnKey;
use crate::services::engine_runtime::relay_state::lease::DeliveryLeaseKey;
use poise::serenity_prelude::ChannelId;

#[cfg(test)]
#[track_caller]
pub(in crate::services::discord) fn from_turn_key(turn: TurnKey) -> DeliveryLeaseKey {
    DeliveryLeaseKey::new_for_site(
        turn.channel_id,
        turn.generation,
        turn.user_msg_id,
        None,
        None,
        "delivery_lease_key.turn",
    )
}

#[cfg(test)]
#[track_caller]
pub(in crate::services::discord) fn from_inflight_state(
    channel_id: ChannelId,
    generation: u64,
    state: &InflightTurnState,
) -> DeliveryLeaseKey {
    from_inflight_state_for_site(channel_id, generation, state, "delivery_lease_key.inflight")
}

#[track_caller]
pub(in crate::services::discord) fn from_inflight_state_for_site(
    channel_id: ChannelId,
    generation: u64,
    state: &InflightTurnState,
    site: &'static str,
) -> DeliveryLeaseKey {
    DeliveryLeaseKey::new_for_site(
        channel_id,
        generation,
        state.user_msg_id,
        Some(&state.started_at),
        state.turn_start_offset,
        site,
    )
}
