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
                "busy follow-up notice is confirmed missing; unbinding it and keeping its retry budget"
            );
            let _ = crate::services::discord::busy_followup_retry_store::unbind_notice_if_current(
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
/// A confirmed-missing or released notice falls through to a replacement card, which the next
/// bind adopts with the same retry budget.
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
    )
    .filter(|binding| binding.notice_bound)?;
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
    // #5035 (A7): "ownerless" holds only for the consumed id — the same card can
    // still represent other waiting entries, so the gate decides. `user_msg_id`
    // is the busy-retry input id, NOT the dispatch head, so the departing hint is
    // left empty (harmless: the hint never changes a verdict).
    if let Some(stale_queued) = queued_placeholder_handoff.filter(|queued| *queued != existing) {
        let stale_disposition =
            match queued_card_gate::release_or_rekey(shared, channel_id, stale_queued, &[]).await {
                QueuedCardDisposition::Released(teardown) => {
                    match queued_card_gate::teardown_delete(http, shared, teardown).await {
                        Ok(()) => "deleted".to_string(),
                        Err(_) => "delete_failed".to_string(),
                    }
                }
                QueuedCardDisposition::Preserved { owner } => {
                    format!("preserved_for_{}", owner.get())
                }
            };
        tracing::info!(
            channel_id = channel_id.get(),
            user_msg_id = user_msg_id.get(),
            notice_message_id = existing.get(),
            stale_queued = stale_queued.get(),
            stale_disposition = stale_disposition,
            "busy follow-up retry reused its bound notice card; routed the queued card through the #5035 gate"
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

        // Each lost card is replaced and the same budget continues until the sixth failure caps.
        use crate::services::discord::busy_followup_retry_store as store;
        for round in 1..store::MAX_BUSY_RETRY_COUNT {
            let card = MessageId::new(notice.get() + u64::from(round));
            let missing = ClassifiedOutboundEditError::ConfirmedMissing("404 (10008)".to_string());
            let previous = MessageId::new(card.get() - 1);
            let result = apply_bound_notice_edit_result(
                &provider,
                channel_id,
                user_msg_id,
                previous,
                Err(missing),
            );
            assert_eq!(result, Err(None));
            let rebound = store::bind_notice_if_absent(
                &provider,
                channel_id.get(),
                user_msg_id.get(),
                card.get(),
            )
            .expect("replacement binds");
            assert_eq!(rebound.notice_message_id, card.get(), "round {round}");
            assert_eq!(rebound.busy_retry_count, round, "round {round}");
            let charged = store::record_busy_retry(
                &provider,
                channel_id.get(),
                user_msg_id.get(),
                card.get(),
            )
            .expect("retry recorded");
            assert_eq!(charged.state.busy_retry_count, round + 1, "round {round}");
            assert_eq!(charged.capped, round + 1 == store::MAX_BUSY_RETRY_COUNT);
        }
        assert!(store::is_capped(
            &provider,
            channel_id.get(),
            user_msg_id.get()
        ));
    }
}
