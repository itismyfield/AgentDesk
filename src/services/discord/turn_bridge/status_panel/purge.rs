use super::super::*;

pub(super) fn purge_pending_bind_for_completed_status_panel(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    status_panel_msg_id: Option<MessageId>,
) {
    let Some(message_id) = normalize_status_panel_message_id(status_panel_msg_id) else {
        return;
    };
    crate::services::discord::status_panel_orphan_store::remove_pending_bind(
        provider,
        &shared.token_hash,
        channel_id.get(),
        message_id.get(),
    );
}

pub(super) fn purge_terminal_reconcile_for_completed_status_panel(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    status_panel_msg_id: Option<MessageId>,
) {
    let Some(message_id) = normalize_status_panel_message_id(status_panel_msg_id) else {
        return;
    };
    crate::services::discord::abandon_request_store::remove(
        provider,
        &shared.token_hash,
        channel_id.get(),
        message_id.get(),
    );
}
