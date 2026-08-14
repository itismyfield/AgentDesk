use super::*;

struct IntakeOutboxArgument(Option<i64>);

impl IntakeOutboxArgument {
    fn from_inflight(state: &InflightTurnState) -> Self {
        Self(state.intake_outbox_id())
    }
}

pub(in crate::services::discord::turn_bridge) struct HeadlessDeliveryArguments<'a> {
    shared: &'a Arc<SharedData>,
    channel_id: ChannelId,
    // `None` for a recovery turn with no anchored user message (user_msg_id == 0).
    owning_user_msg_id: Option<MessageId>,
    session_key: Option<&'a str>,
    delivery_bot: Option<&'a str>,
    // Parked for the follow-up slice that will bind delivery rows to their
    // intake outbox owner. It has no runtime effect in this slice.
    _intake_outbox_id: IntakeOutboxArgument,
    provider: &'a ProviderKind,
    content: &'a str,
    cancel_token: Option<&'a CancelToken>,
}

pub(in crate::services::discord::turn_bridge) struct HeadlessDeliveryInputs<'a> {
    pub(in crate::services::discord::turn_bridge) shared: &'a Arc<SharedData>,
    pub(in crate::services::discord::turn_bridge) channel_id: ChannelId,
    pub(in crate::services::discord::turn_bridge) owning_user_msg_id: Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) session_key: Option<&'a str>,
    pub(in crate::services::discord::turn_bridge) provider: &'a ProviderKind,
    pub(in crate::services::discord::turn_bridge) content: &'a str,
    pub(in crate::services::discord::turn_bridge) cancel_token: Option<&'a CancelToken>,
}

pub(super) struct HeadlessDeliveryRuntimeArguments<'a> {
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: ChannelId,
    pub(super) owning_user_msg_id: Option<MessageId>,
    pub(super) session_key: Option<&'a str>,
    pub(super) delivery_bot: Option<&'a str>,
    pub(super) provider: &'a ProviderKind,
    pub(super) content: &'a str,
    pub(super) cancel_token: Option<&'a CancelToken>,
}

impl<'a> HeadlessDeliveryArguments<'a> {
    pub(super) fn into_runtime_arguments(self) -> HeadlessDeliveryRuntimeArguments<'a> {
        let IntakeOutboxArgument(intake_outbox_id) = self._intake_outbox_id;
        let _ = intake_outbox_id;
        HeadlessDeliveryRuntimeArguments {
            shared: self.shared,
            channel_id: self.channel_id,
            owning_user_msg_id: self.owning_user_msg_id,
            session_key: self.session_key,
            delivery_bot: self.delivery_bot,
            provider: self.provider,
            content: self.content,
            cancel_token: self.cancel_token,
        }
    }
}

pub(in crate::services::discord::turn_bridge) fn assemble_headless_delivery_arguments<'a>(
    inflight_state: &'a InflightTurnState,
    inputs: HeadlessDeliveryInputs<'a>,
) -> HeadlessDeliveryArguments<'a> {
    HeadlessDeliveryArguments {
        shared: inputs.shared,
        channel_id: inputs.channel_id,
        owning_user_msg_id: inputs.owning_user_msg_id,
        session_key: inputs.session_key,
        delivery_bot: inflight_state.delivery_bot.as_deref(),
        _intake_outbox_id: IntakeOutboxArgument::from_inflight(inflight_state),
        provider: inputs.provider,
        content: inputs.content,
        cancel_token: inputs.cancel_token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    #[test]
    fn delivery_argument_carries_inflight_intake_outbox_identity() {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            42,
            Some("adk-cdx".to_string()),
            7,
            8,
            9,
            "hello".to_string(),
            None,
            Some("AgentDesk-claude-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        state.adopt_intake_outbox(Some(5071));

        assert_eq!(IntakeOutboxArgument::from_inflight(&state).0, Some(5071));
    }
}
