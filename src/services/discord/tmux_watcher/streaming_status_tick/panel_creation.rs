use super::*;

pub(in crate::services::discord::tmux::tmux_watcher) fn status_panel_send_is_due(
    prepared: Option<
        &crate::services::discord::status_panel_transition::PreparedStatusPanelTransition,
    >,
) -> bool {
    prepared.is_none_or(|prepared| prepared.send_now)
}

pub(in crate::services::discord::tmux::tmux_watcher) fn status_panel_send_payload<'a>(
    prepared: Option<
        &'a crate::services::discord::status_panel_transition::PreparedStatusPanelTransition,
    >,
    fallback: &'a str,
) -> &'a str {
    prepared.map_or(fallback, |prepared| prepared.content.as_str())
}

pub(in crate::services::discord::tmux::tmux_watcher) fn record_status_panel_send_failure(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    prepared: Option<
        &crate::services::discord::status_panel_transition::PreparedStatusPanelTransition,
    >,
    error: &serenity::Error,
) -> Option<crate::services::discord::status_panel_transition::StatusPanelCreateFailureDisposition>
{
    prepared.and_then(|prepared| {
        match crate::services::discord::status_panel_transition::record_serenity_create_failure(
            provider,
            &shared.token_hash,
            channel_id.get(),
            prepared,
            error,
        ) {
            Ok(disposition) => Some(disposition),
            Err(durability_error) => {
                tracing::warn!(
                    channel_id = channel_id.get(),
                    error = %durability_error,
                    "watcher could not persist status-panel send disposition"
                );
                None
            }
        }
    })
}
