use super::*;

pub(super) struct RecoveryKickoffIdentity {
    pub(super) request_owner: UserId,
    pub(super) user_message_id: Option<MessageId>,
}

pub(super) fn recovery_kickoff_identity(
    state: &inflight::InflightTurnState,
) -> Option<RecoveryKickoffIdentity> {
    Some(RecoveryKickoffIdentity {
        request_owner: UserId::new(state.request_owner_user_id),
        user_message_id: optional_message_id(state.user_msg_id),
    })
}
