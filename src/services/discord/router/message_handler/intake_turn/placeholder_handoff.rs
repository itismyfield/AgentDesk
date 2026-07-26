use super::*;

fn apply_bound_notice_edit_result(
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    existing: MessageId,
    result: Result<(), super::super::super::super::gateway::ClassifiedOutboundEditError>,
) -> Result<(), Option<MessageId>> {
    match result {
        Ok(()) => Ok(()),
        Err(
            super::super::super::super::gateway::ClassifiedOutboundEditError::ConfirmedMissing(
                error,
            ),
        ) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                user_msg_id = user_msg_id.get(),
                notice_message_id = existing.get(),
                error = %error,
                "busy follow-up notice is confirmed missing; clearing its binding before replacement"
            );
            let _ = crate::services::discord::busy_followup_retry_store::clear_if_current(
                provider,
                channel_id.get(),
                user_msg_id.get(),
                existing.get(),
            );
            Err(None)
        }
        Err(super::super::super::super::gateway::ClassifiedOutboundEditError::Other(error)) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                user_msg_id = user_msg_id.get(),
                notice_message_id = existing.get(),
                error = %error,
                "busy follow-up notice edit outcome is ambiguous; preserving binding and retry budget"
            );
            Err(Some(existing))
        }
    }
}

/// Reuse the retry input's bound busy notice rather than posting a card per attempt.
/// A confirmed-missing notice clears its stale binding and falls through to replacement.
pub(super) async fn reuse_bound_busy_notice(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    queued_placeholder_handoff: Option<MessageId>,
) -> Option<MessageId> {
    let binding = crate::services::discord::busy_followup_retry_store::load(
        provider,
        channel_id.get(),
        user_msg_id.get(),
    )?;
    let existing = MessageId::new(binding.notice_message_id);
    match apply_bound_notice_edit_result(
        provider,
        channel_id,
        user_msg_id,
        existing,
        super::super::super::super::gateway::edit_intake_placeholder(
            http.clone(),
            shared.clone(),
            channel_id,
            existing,
        )
        .await,
    ) {
        Ok(()) => {}
        Err(existing) => return existing,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::gateway::ClassifiedOutboundEditError;

    #[test]
    fn only_confirmed_missing_replaces_bound_busy_notice_4888() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(100_000_004_888_401);
        let user_msg_id = MessageId::new(100_000_004_888_402);
        let notice = MessageId::new(100_000_004_888_403);
        crate::services::discord::busy_followup_retry_store::bind_notice_if_absent(
            &provider,
            channel_id.get(),
            user_msg_id.get(),
            notice.get(),
        )
        .expect("bind notice");
        crate::services::discord::busy_followup_retry_store::record_busy_retry(
            &provider,
            channel_id.get(),
            user_msg_id.get(),
            notice.get(),
        )
        .expect("seed aggregate retry count");

        for reason in [
            "429 rate limited",
            "500 server error",
            "503 unavailable",
            "network timeout",
        ] {
            assert_eq!(
                apply_bound_notice_edit_result(
                    &provider,
                    channel_id,
                    user_msg_id,
                    notice,
                    Err(ClassifiedOutboundEditError::Other(reason.to_string())),
                ),
                Err(Some(notice))
            );
            let preserved = crate::services::discord::busy_followup_retry_store::load(
                &provider,
                channel_id.get(),
                user_msg_id.get(),
            )
            .expect("ambiguous failure preserves binding");
            assert_eq!(preserved.notice_message_id, notice.get());
            assert_eq!(preserved.busy_retry_count, 1);
        }

        assert_eq!(
            apply_bound_notice_edit_result(
                &provider,
                channel_id,
                user_msg_id,
                notice,
                Err(ClassifiedOutboundEditError::ConfirmedMissing(
                    "404 Unknown Message (10008)".to_string(),
                )),
            ),
            Err(None)
        );
        assert!(
            crate::services::discord::busy_followup_retry_store::load(
                &provider,
                channel_id.get(),
                user_msg_id.get(),
            )
            .is_none(),
            "only authoritative missing permits the replacement path"
        );
    }
}
