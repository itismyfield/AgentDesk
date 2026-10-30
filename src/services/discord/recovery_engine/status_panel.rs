use poise::serenity_prelude::{ChannelId, MessageId};

use super::super::inflight::opt_message_id;
use super::super::{inflight, turn_bridge};
use crate::services::provider::ProviderKind;

pub(super) fn completion_target(
    status_panel_v2_enabled: bool,
    state: &inflight::InflightTurnState,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<Option<MessageId>> {
    if !status_panel_v2_enabled {
        return None;
    }
    let persisted = inflight::load_inflight_state(provider, channel_id.get());
    retired_completion_target(message_id_for_completion(state, persisted.as_ref()))
}

#[cfg(test)]
pub(super) fn completion_target_for_flags(
    status_panel_v2_enabled: bool,
    state: &inflight::InflightTurnState,
    persisted: Option<&inflight::InflightTurnState>,
) -> Option<Option<MessageId>> {
    if !status_panel_v2_enabled {
        return None;
    }
    retired_completion_target(message_id_for_completion(state, persisted))
}

fn retired_completion_target(message_id: Option<MessageId>) -> Option<Option<MessageId>> {
    message_id.map(Some)
}

pub(super) fn message_id_for_completion(
    state: &inflight::InflightTurnState,
    persisted: Option<&inflight::InflightTurnState>,
) -> Option<MessageId> {
    persisted
        .and_then(|inflight| {
            if inflight.user_msg_id == state.user_msg_id {
                turn_bridge::normalize_status_panel_message_id(
                    inflight.status_message_id.and_then(opt_message_id),
                )
            } else {
                None
            }
        })
        .or_else(|| {
            turn_bridge::normalize_status_panel_message_id(
                state.status_message_id.and_then(opt_message_id),
            )
        })
}

#[cfg(test)]
#[path = "turn_summary_recovery_tests.rs"]
mod turn_summary_recovery_tests;

#[cfg(test)]
#[path = "footer_retirement_tests.rs"]
mod footer_retirement_tests;
