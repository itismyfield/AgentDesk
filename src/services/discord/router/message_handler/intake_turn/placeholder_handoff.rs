use super::*;

pub(super) async fn reuse_bound_busy_notice(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    queued_placeholder_handoff: Option<MessageId>,
) -> Option<MessageId> {
    let binding = super::super::super::super::busy_followup_retry_store::load(
        provider,
        channel_id.get(),
        user_msg_id.get(),
    )?;
    let existing = MessageId::new(binding.notice_message_id);
    if let Err(error) = super::super::super::super::gateway::edit_intake_placeholder(
        http.clone(),
        shared.clone(),
        channel_id,
        existing,
    )
    .await
    {
        tracing::warn!(
            channel_id = channel_id.get(),
            user_msg_id = user_msg_id.get(),
            notice_message_id = existing.get(),
            error = %error,
            "busy follow-up notice edit failed; dropping the stale binding and posting a fresh anchor"
        );
        let _ = super::super::super::super::busy_followup_retry_store::clear_if_current(
            provider,
            channel_id.get(),
            user_msg_id.get(),
            existing.get(),
        );
        return None;
    }

    // The dispatch hand-off already consumed this message's queued-placeholder
    // mapping, so a distinct queued card would otherwise remain ownerless.
    if let Some(stale_queued) = queued_placeholder_handoff.filter(|queued| *queued != existing) {
        let deleted = channel_id.delete_message(http, stale_queued).await;
        shared
            .ui
            .placeholder_controller
            .detach_by_message(channel_id, stale_queued);
        tracing::info!(
            channel_id = channel_id.get(),
            user_msg_id = user_msg_id.get(),
            notice_message_id = existing.get(),
            stale_queued = stale_queued.get(),
            stale_deleted = deleted.is_ok(),
            "busy follow-up retry reused its bound notice card; dropped the orphaned queued card"
        );
    }
    Some(existing)
}
