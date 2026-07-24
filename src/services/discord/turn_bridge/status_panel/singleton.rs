use super::super::*;

pub(super) fn commit_completed_binding(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    panel_message_id: Option<MessageId>,
) -> bool {
    if !shared.ui.two_message_panel_enabled {
        return true;
    }
    let Some(panel_message_id) = normalize_status_panel_message_id(panel_message_id) else {
        return true;
    };
    match crate::services::discord::status_panel_singleton_store::commit_if_owned_or_current(
        provider,
        &shared.token_hash,
        channel_id.get(),
        panel_message_id.get(),
    ) {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                panel_message_id = panel_message_id.get(),
                error = %error,
                "failed to durably commit completed two-message singleton panel"
            );
            false
        }
    }
}
