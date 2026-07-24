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
    let generation =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
            .filter(|state| state.status_message_id == Some(panel_message_id.get()))
            .map(|state| state.status_panel_generation)
            .or_else(|| {
                crate::services::discord::status_panel_singleton_store::load(
                    provider,
                    &shared.token_hash,
                    channel_id.get(),
                )
                .map(|binding| binding.generation)
            })
            .unwrap_or_default();
    match crate::services::discord::status_panel_singleton_store::bind(
        provider,
        &shared.token_hash,
        channel_id.get(),
        panel_message_id.get(),
        generation,
    ) {
        Ok(()) => true,
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
