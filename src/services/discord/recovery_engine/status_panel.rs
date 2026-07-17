use poise::serenity_prelude::{ChannelId, MessageId};

use super::super::inflight::opt_message_id;
use super::super::{inflight, single_message_panel, turn_bridge};
use crate::services::provider::ProviderKind;

pub(super) fn completion_target(
    status_panel_v2_enabled: bool,
    state: &inflight::InflightTurnState,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<Option<MessageId>> {
    if !single_message_panel::separate_status_panel_enabled(status_panel_v2_enabled) {
        return None;
    }
    let persisted = inflight::load_inflight_state(provider, channel_id.get());
    Some(message_id_for_completion(state, persisted.as_ref()))
}

#[cfg(test)]
pub(super) fn completion_target_for_flags(
    single_message_panel_enabled_flag: bool,
    status_panel_v2_enabled: bool,
    state: &inflight::InflightTurnState,
    persisted: Option<&inflight::InflightTurnState>,
) -> Option<Option<MessageId>> {
    if !single_message_panel::separate_status_panel_enabled_for_flags(
        single_message_panel_enabled_flag,
        status_panel_v2_enabled,
    ) {
        return None;
    }
    Some(message_id_for_completion(state, persisted))
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
mod tests {
    use super::super::super::single_message_panel_enabled;
    use super::*;

    #[test]
    fn current_flag_helper_matches_for_flags() {
        assert_eq!(
            single_message_panel::separate_status_panel_enabled_for_flags(
                single_message_panel_enabled(),
                true,
            ),
            single_message_panel::separate_status_panel_enabled(true)
        );
    }
}
