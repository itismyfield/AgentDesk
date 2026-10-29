use super::*;

pub(in crate::services::discord) async fn requeue_claude_tui_followup_pre_submit_timeout(
    shared_owned: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
    dispatch_id: Option<&str>,
    adk_session_key: Option<&str>,
    attempt: BusyAttempt<'_>,
) -> FollowupRequeueOutcome {
    let held = inflight_state
        .tmux_session_name
        .as_deref()
        .filter(|session| crate::services::claude_tui::composer_lock::draft_guarded(session));
    let Some(notice_message_id) = ensure_busy_notice(
        shared_owned,
        provider,
        channel_id,
        inflight_state,
        held.is_none(),
    )
    .await
    else {
        return FollowupRequeueOutcome {
            requeued: false,
            retry_capped: false,
            notice_message_id: None,
            preparation_held: held.is_some(),
            notice_projection_pending: true,
        };
    };
    let mut sources = inflight_state.source_message_ids.clone();
    sources.push(inflight_state.user_msg_id);
    let retry_decision = match super::super::busy_followup_retry_store::record_attempt(
        provider,
        (
            channel_id.get(),
            inflight_state.effective_busy_followup_retry_user_msg_id(),
            notice_message_id.get(),
        ),
        &sources,
        attempt.cost,
        held,
    ) {
        Ok(decision) => decision,
        // An unrecorded failure must not become a free retry: keep the inflight turn instead.
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                error = %error,
                "busy follow-up retry budget not recorded; preserving inflight without requeue"
            );
            return FollowupRequeueOutcome {
                preparation_held: held.is_some(),
                requeued: false,
                retry_capped: false,
                notice_message_id: Some(notice_message_id),
                notice_projection_pending: false,
            };
        }
    };
    if retry_decision.capped {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            user_msg_id = inflight_state.effective_busy_followup_retry_user_msg_id(),
            "Claude TUI busy follow-up aggregate retry cap reached; preserving entry without kickoff"
        );
    }
    let requeue_outcome = super::super::mailbox_requeue_inflight_for_followup_retry(
        shared_owned,
        provider,
        channel_id,
        inflight_state,
    )
    .await;
    let requeue_refusal_reason = requeue_outcome.refusal_reason.map(|reason| reason.as_str());
    tracing::info!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        user_msg_id = inflight_state.effective_busy_followup_retry_user_msg_id(),
        requeue_enqueued = requeue_outcome.enqueued,
        requeue_merged = requeue_outcome.merged,
        requeue_refusal_reason = requeue_refusal_reason.unwrap_or("none"),
        requeue_persistence_error = requeue_outcome.persistence_error.as_deref().unwrap_or("none"),
        "claude_tui follow-up pre-submit timeout: requeue attempt completed"
    );
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        dispatch_id,
        adk_session_key,
        Some(attempt.turn_id),
        "claude_tui_followup_pre_submit_requeue",
        serde_json::json!({
            "user_msg_id": inflight_state.effective_busy_followup_retry_user_msg_id(),
            "requeue_enqueued": requeue_outcome.enqueued,
            "requeue_merged": requeue_outcome.merged,
            "requeue_refusal_reason": requeue_refusal_reason,
            "requeue_persistence_error": requeue_outcome.persistence_error,
        }),
    );

    let retry_present_or_accepted = retry_present_or_accepted(&requeue_outcome);
    if retry_present_or_accepted {
        if should_publish_queue_marker(&requeue_outcome)
            && let Some(http) = shared_owned.serenity_http_or_token_fallback()
        {
            let message_id =
                MessageId::new(inflight_state.effective_busy_followup_retry_user_msg_id());
            let queued_generation = super::super::mailbox_snapshot(shared_owned, channel_id)
                .await
                .intervention_queue
                .iter()
                .find_map(|intervention| {
                    intervention
                        .source_message_queued_generations()
                        .into_iter()
                        .find(|source| source.message_id == message_id)
                        .map(|source| source.queued_generation)
                })
                .unwrap_or(shared_owned.restart.current_generation);
            let queue_marker = if requeue_outcome.merged {
                super::super::queue_reactions::QUEUE_MERGED_PENDING_REACTION
            } else {
                super::super::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION
            };
            let delivered = super::super::queue_marker::note_added_queued_generation(
                shared_owned,
                &http,
                channel_id,
                message_id,
                queue_marker,
                queued_generation,
                "claude_tui_followup_requeue_inflight",
            )
            .await;
            super::super::outbound::reaction_control::ensure_queue_reaction_or_fallback_http(
                &http,
                channel_id,
                shared_owned,
                message_id,
                delivered,
            )
            .await;
            let still_queued = super::super::mailbox_snapshot(shared_owned, channel_id)
                .await
                .intervention_queue
                .iter()
                .any(|intervention| {
                    intervention.message_id == message_id
                        || intervention.source_message_ids.contains(&message_id)
                });
            if !still_queued {
                super::super::queue_marker::note_removed_queued_generation(
                    shared_owned,
                    &http,
                    channel_id,
                    message_id,
                    queue_marker,
                    queued_generation,
                    "claude_tui_followup_requeue_self_heal",
                )
                .await;
            }
        }
    }
    // Observe held sources even when earlier busy attempts exhausted their budget.
    if let Some(session) = held.filter(|_| retry_present_or_accepted) {
        let (shared, provider) = (shared_owned.clone(), provider.clone());
        watch_draft_release(shared, provider, channel_id, session.to_string());
    }
    FollowupRequeueOutcome {
        preparation_held: held.is_some(),
        requeued: retry_present_or_accepted,
        retry_capped: retry_decision.capped,
        notice_message_id: Some(notice_message_id),
        notice_projection_pending: false,
    }
}

async fn ensure_busy_notice(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    row: &InflightTurnState,
    allow_create: bool,
) -> Option<MessageId> {
    let input = row.effective_busy_followup_retry_user_msg_id();
    if let Some(binding) =
        super::super::busy_followup_retry_store::load(provider, channel.get(), input)
    {
        return super::super::inflight::opt_message_id(binding.notice_message_id);
    }
    let http = shared.serenity_http_or_token_fallback()?;
    let gateway =
        super::super::gateway::DiscordGateway::new(http, shared.clone(), provider.clone(), None);
    let mut legacy = None;
    if let Some(current) = super::super::inflight::opt_message_id(row.current_msg_id) {
        for marker in ["⠋ 계속 처리 중", "..."] {
            if gateway
                .verify_body_message(shared, channel, current, marker, None)
                .await
                .is_ok()
            {
                legacy = Some(current);
                break;
            }
        }
    }
    let actual =
        match legacy {
            Some(id) => id,
            None => {
                if !allow_create {
                    return None;
                }
                #[cfg(unix)]
                {
                    super::super::body_create::create_notice(shared, &gateway, row,
                    "⏳ Claude TUI가 이전 턴을 처리 중이라 이번 메시지는 아직 주입되지 않았습니다.",
                    super::super::body_create::NoticeScope::BusyFollowup).await.ok()?
                }
                #[cfg(not(unix))]
                return None;
            }
        };
    match super::super::busy_followup_retry_store::bind_notice_if_absent(
        provider,
        channel.get(),
        input,
        actual.get(),
    ) {
        Ok(binding) => super::super::inflight::opt_message_id(binding.notice_message_id),
        Err(error) => {
            tracing::warn!(%error, channel_id=channel.get(), "busy notice retained without a budget binding");
            None
        }
    }
}
