use super::super::*;

/// Durably re-point the channel's singleton binding at the panel that just
/// received its completion footer.
///
/// #4891: this is bookkeeping ONLY. A failure here must never be reported as a
/// completion failure, because the Discord-visible completion edit has already
/// landed by the time this runs — callers treat a failed completion as "the
/// panel is still mid-turn and must be reclaimed", which used to delete the
/// freshly completed panel via the orphan sweeper.
pub(super) fn commit_completed_binding(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    panel_message_id: Option<MessageId>,
) {
    if !shared.ui.two_message_panel_enabled {
        return;
    }
    let Some(panel_message_id) = normalize_status_panel_message_id(panel_message_id) else {
        return;
    };
    if let Err(error) =
        crate::services::discord::status_panel_singleton_store::commit_if_owned_or_current(
            provider,
            &shared.token_hash,
            channel_id.get(),
            panel_message_id.get(),
        )
    {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            panel_message_id = panel_message_id.get(),
            error = %error,
            "failed to durably commit completed two-message singleton panel; \
             keeping the live completed panel (#4891)"
        );
    }
}
