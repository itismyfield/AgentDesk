use poise::serenity_prelude::MessageId;

use crate::services::discord::inflight::InflightTurnState;

/// Health endpoints expose Discord message IDs only when they identify a real
/// Discord message. Rebind-origin placeholder IDs are stored as zero and must
/// stay out of operator-facing JSON.
pub(super) fn visible_message_id(raw_id: u64) -> Option<u64> {
    (raw_id != 0).then_some(raw_id)
}

pub(super) fn visible_serenity_message_id(message_id: Option<MessageId>) -> Option<u64> {
    message_id.and_then(|id| visible_message_id(id.get()))
}

pub(super) fn visible_inflight_user_msg_id(inflight: Option<&InflightTurnState>) -> Option<u64> {
    inflight.and_then(|state| visible_message_id(state.user_msg_id))
}

pub(super) fn visible_inflight_current_msg_id(inflight: Option<&InflightTurnState>) -> Option<u64> {
    inflight.and_then(|state| visible_message_id(state.current_msg_id))
}

#[cfg(test)]
mod tests {
    use super::visible_message_id;

    #[test]
    fn visible_message_id_redacts_zero_placeholders() {
        assert_eq!(visible_message_id(0), None);
        assert_eq!(visible_message_id(42), Some(42));
    }
}
