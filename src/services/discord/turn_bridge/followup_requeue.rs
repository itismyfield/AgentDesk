use super::*;

fn should_publish_queue_marker(outcome: &crate::services::discord::MailboxEnqueueOutcome) -> bool {
    !matches!(
        outcome.refusal_reason,
        Some(
            crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued
                | crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdPendingOrActive
        )
    )
}

fn retry_present_or_accepted(outcome: &crate::services::discord::MailboxEnqueueOutcome) -> bool {
    outcome.enqueued
        || matches!(
            outcome.refusal_reason,
            Some(
                crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued
                    | crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdPendingOrActive
                    | crate::services::turn_orchestrator::EnqueueRefusalReason::LastItemDedup
            )
        )
}

pub(super) async fn requeue_claude_tui_followup_pre_submit_timeout(
    shared_owned: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
    dispatch_id: Option<&str>,
    adk_session_key: Option<&str>,
    turn_id: &str,
) -> bool {
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
        user_msg_id = inflight_state.user_msg_id,
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
        Some(turn_id),
        "claude_tui_followup_pre_submit_requeue",
        serde_json::json!({
            "user_msg_id": inflight_state.user_msg_id,
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
            let message_id = MessageId::new(inflight_state.user_msg_id);
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
        super::super::schedule_deferred_idle_queue_kickoff(
            shared_owned.clone(),
            provider.clone(),
            channel_id,
            "claude_tui_followup_requeue_inflight",
        );
    }
    retry_present_or_accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        ScopedRuntimeRoot {
            _lock: lock,
            _temp: temp,
            previous,
        }
    }

    fn inflight(channel_id: ChannelId, message_id: MessageId) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            channel_id.get(),
            Some("adk-cc".to_string()),
            42,
            message_id.get(),
            message_id.get() + 1,
            "queued follow-up".to_string(),
            Some("session-4248".to_string()),
            Some("AgentDesk-claude-4248".to_string()),
            Some("/tmp/agentdesk-4248.jsonl".to_string()),
            None,
            0,
        )
    }

    #[test]
    fn already_queued_refusal_preserves_existing_merged_marker() {
        let outcome = crate::services::discord::MailboxEnqueueOutcome {
            refusal_reason: Some(
                crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued,
            ),
            ..Default::default()
        };

        assert!(
            !should_publish_queue_marker(&outcome),
            "duplicate refusal must not rewrite the live queue entry's merged/standalone marker"
        );
        assert!(
            retry_present_or_accepted(&outcome),
            "existing queue entry makes the retry safe to report"
        );
    }

    #[test]
    fn persistence_failure_is_not_reported_as_requeued() {
        let outcome = crate::services::discord::MailboxEnqueueOutcome {
            persistence_error: Some("pending queue write failed".to_string()),
            ..Default::default()
        };

        assert!(
            !retry_present_or_accepted(&outcome),
            "a failed queue write must preserve inflight rather than report requeue success"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pre_submit_retry_reaction_failure_emits_exactly_one_referenced_fallback() {
        let _root = scoped_runtime_root();
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .expect("fresh shared data")
            .turn_view_reconciler =
            crate::services::discord::turn_view_reconciler::TurnViewReconciler::with_test_deliveries(
                vec![crate::services::discord::turn_view_reconciler::TurnViewDelivery::Failed],
            );
        shared
            .http
            .cached_bot_token
            .set("Bot test-token".to_string())
            .expect("test bot token");
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(100_000_004_248_003);
        let message_id = MessageId::new(100_000_004_248_004);
        let inflight = inflight(channel_id, message_id);
        assert!(
            crate::services::discord::outbound::reaction_control::take_test_reply_deliveries()
                .is_empty()
        );

        assert!(
            requeue_claude_tui_followup_pre_submit_timeout(
                &shared,
                &provider,
                channel_id,
                &inflight,
                None,
                None,
                "turn-4248-reaction-failure",
            )
            .await
        );

        assert_eq!(
            crate::services::discord::outbound::reaction_control::take_test_reply_deliveries(),
            vec![crate::services::discord::outbound::reaction_control::ReactionControlReplyReason::QueueReactionFailed],
            "failed follow-up requeue reaction must emit exactly one referenced fallback"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pre_submit_retry_adds_queue_reaction_immediately_through_reconciler() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared
            .http
            .cached_bot_token
            .set("Bot test-token".to_string())
            .expect("test bot token");
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(100_000_004_248_001);
        let message_id = MessageId::new(100_000_004_248_002);
        let inflight = inflight(channel_id, message_id);

        requeue_claude_tui_followup_pre_submit_timeout(
            &shared,
            &provider,
            channel_id,
            &inflight,
            None,
            None,
            "turn-4248",
        )
        .await;

        let ops = shared.turn_view_reconciler.ops();
        assert!(
            !ops.iter()
                .any(|op| { op.target.message_id == message_id && op.add && op.emoji == '⏳' }),
            "queued retry must publish only its queue-kind marker"
        );
        assert!(ops.iter().any(|op| {
            op.target.message_id == message_id
                && op.add
                && matches!(
                    op.emoji,
                    crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION
                        | crate::services::discord::queue_reactions::QUEUE_MERGED_PENDING_REACTION
                )
        }));
        assert!(
            ops.iter().all(|op| op.identity == "intake"),
            "retry queue reaction must retain one reconciler-owned intake identity"
        );
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert!(snapshot.intervention_queue.iter().any(|intervention| {
            intervention.message_id == message_id
                || intervention.source_message_ids.contains(&message_id)
        }));
    }
}
