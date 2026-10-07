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

#[derive(Clone, Copy)]
pub(super) struct FollowupRequeueOutcome {
    pub(super) requeued: bool,
    pub(super) retry_capped: bool,
    pub(super) notice_message_id: MessageId,
}

/// Proof produced only after the bounded terminal projection future settles.
/// The private field prevents callers from fabricating the ordering boundary.
pub(super) struct TerminalProjectionSettled {
    _private: (),
}

impl TerminalProjectionSettled {
    pub(super) async fn after<F, T>(projection: F) -> (Self, T)
    where
        F: std::future::Future<Output = T>,
    {
        let output = projection.await;
        (Self { _private: () }, output)
    }

    pub(super) fn release_completion_admission(
        self,
        completion_guard: &super::guards::CompletionGuard,
        outcome: Option<FollowupRequeueOutcome>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        reason: &'static str,
    ) -> bool {
        let queue_eligible =
            outcome.is_none_or(|outcome| outcome.requeued && !outcome.retry_capped);
        completion_guard.note_terminal_projection_settled(true);
        completion_guard.note_terminal_disposition_settled(queue_eligible);
        schedule_retry_if_eligible(outcome, shared, provider, channel_id, reason)
    }
}

fn schedule_retry_if_eligible(
    outcome: Option<FollowupRequeueOutcome>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) -> bool {
    let Some(outcome) = outcome else {
        return false;
    };
    if !outcome.requeued || outcome.retry_capped {
        return false;
    }
    super::super::schedule_deferred_idle_queue_kickoff(
        shared.clone(),
        provider.clone(),
        channel_id,
        reason,
    );
    true
}

pub(super) async fn requeue_if_needed(
    outcome: &mut Option<FollowupRequeueOutcome>,
    requeue_candidate: bool,
    already_pending: bool,
    shared_owned: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
    dispatch_id: Option<&str>,
    adk_session_key: Option<&str>,
    turn_id: &str,
) {
    if !requeue_candidate || already_pending {
        return;
    }
    *outcome = Some(
        requeue_claude_tui_followup_pre_submit_timeout(
            shared_owned,
            provider,
            channel_id,
            inflight_state,
            dispatch_id,
            adk_session_key,
            turn_id,
        )
        .await,
    );
}

pub(super) async fn requeue_claude_tui_followup_pre_submit_timeout(
    shared_owned: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
    dispatch_id: Option<&str>,
    adk_session_key: Option<&str>,
    turn_id: &str,
) -> FollowupRequeueOutcome {
    let notice_message_id = super::super::busy_followup_retry_store::bind_notice_if_absent(
        provider,
        channel_id.get(),
        inflight_state.effective_busy_followup_retry_user_msg_id(),
        inflight_state.current_msg_id,
    )
    .map(|state| MessageId::new(state.notice_message_id))
    .unwrap_or_else(|error| {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            user_msg_id = inflight_state.effective_busy_followup_retry_user_msg_id(),
            error = %error,
            "failed to bind busy follow-up notice; using current placeholder"
        );
        MessageId::new(inflight_state.current_msg_id)
    });
    // A pane protecting a person's draft refused this input, not a busy turn.
    let held = inflight_state
        .tmux_session_name
        .as_deref()
        .filter(|session| crate::services::claude_tui::composer_lock::draft_guarded(session));
    let record = match held {
        Some(_) => super::super::busy_followup_retry_store::record_draft_hold_retry,
        None => super::super::busy_followup_retry_store::record_busy_retry,
    };
    let retry_decision = record(
        provider,
        channel_id.get(),
        inflight_state.effective_busy_followup_retry_user_msg_id(),
        notice_message_id.get(),
    )
    .ok();
    if retry_decision.is_some_and(|decision| decision.capped) {
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
        Some(turn_id),
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
    let retry_capped = retry_decision.is_some_and(|decision| decision.capped);
    if let Some(session) = held.filter(|_| retry_capped && retry_present_or_accepted) {
        let (shared, provider) = (shared_owned.clone(), provider.clone());
        watch_draft_release(shared, provider, channel_id, session.to_string());
    }
    FollowupRequeueOutcome {
        requeued: retry_present_or_accepted,
        retry_capped,
        notice_message_id,
    }
}

/// How often a capped input held by a person's draft looks for that protection to lift.
const DRAFT_RELEASE_POLL: std::time::Duration = std::time::Duration::from_secs(5);
/// The retry store keeps a binding this long, so nothing is left to resume after it.
const DRAFT_RELEASE_WATCH: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

static DRAFT_RELEASE_WATCHES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(u64, String)>>,
> = std::sync::LazyLock::new(Default::default);

fn draft_release_watches()
-> std::sync::MutexGuard<'static, std::collections::HashSet<(u64, String)>> {
    DRAFT_RELEASE_WATCHES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// One watch per channel and pane: once the pane protects no draft, the held inputs get a fresh
/// budget and the queue one kickoff.
fn watch_draft_release(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    session: String,
) {
    let key = (channel_id.get(), session.clone());
    if !draft_release_watches().insert(key.clone()) {
        return;
    }
    tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        while started.elapsed() < DRAFT_RELEASE_WATCH {
            tokio::time::sleep(DRAFT_RELEASE_POLL).await;
            if draft_lifted(&session).await {
                resume_after_draft_release(&shared, &provider, channel_id);
                break;
            }
        }
        draft_release_watches().remove(&key);
    });
}

/// A pane still marked protected is read off the runtime, between composer mutations.
async fn draft_lifted(session: &str) -> bool {
    use crate::services::claude_tui::composer_lock::{draft_guarded, draft_released};
    if !draft_guarded(session) {
        return true;
    }
    let session = session.to_string();
    let released = tokio::task::spawn_blocking(move || draft_released(&session)).await;
    released.unwrap_or(false)
}

fn resume_after_draft_release(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> bool {
    use super::super::busy_followup_retry_store::release_draft_holds;
    let released = release_draft_holds(provider, channel_id.get());
    tracing::info!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        released,
        "claude_tui draft protection lifted; held follow-ups resume"
    );
    if released == 0 {
        return false;
    }
    let reason = "claude_tui_draft_hold_released";
    super::super::schedule_deferred_idle_queue_kickoff(
        shared.clone(),
        provider.clone(),
        channel_id,
        reason,
    );
    true
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

    #[tokio::test(flavor = "current_thread")]
    async fn c1b_timeout_consumer_uses_preclose_bridge_effect_for_binding_and_all_sources() {
        use crate::services::discord::input_runtime::fence::{self, effect};
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(6_325_519);
        let mut state = inflight(channel, MessageId::new(6_325_520));
        state.source_message_ids = vec![6_325_521, 6_325_522];
        let gate = fence::Gate::protect(provider.clone(), channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let permit = gate.admit().unwrap();
        let closing = gate.close().unwrap();
        let result = effect::run(Some(permit), {
            let shared = shared.clone();
            let provider = provider.clone();
            async move {
                requeue_claude_tui_followup_pre_submit_timeout(
                    &shared,
                    &provider,
                    channel,
                    &state,
                    None,
                    None,
                    "c1b-preclose-timeout",
                )
                .await
            }
        })
        .await;
        assert!(result.requeued);
        assert!(!result.retry_capped);
        assert_eq!(result.notice_message_id, MessageId::new(6_325_521));
        let binding = super::super::super::busy_followup_retry_store::load(
            &provider,
            channel.get(),
            6_325_520,
        )
        .unwrap();
        assert_eq!(binding.busy_retry_count, 1);
        assert_eq!(binding.notice_message_id, 6_325_521);
        let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &provider,
            &shared.token_hash,
            channel,
        );
        assert_eq!(disk.len(), 1);
        assert_eq!(
            disk[0].source_message_ids,
            vec![
                MessageId::new(6_325_521),
                MessageId::new(6_325_522),
                MessageId::new(6_325_520),
            ]
        );
        closing.drain().await;
        assert!(matches!(
            gate.admit(),
            Err(fence::Failure::Mode(fence::Mode::Closing))
        ));
        assert!(effect::current().is_none());
        shared.mailboxes.remove_fixture_for_test(channel);
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
            .requeued
        );

        assert_eq!(
            crate::services::discord::outbound::reaction_control::take_test_reply_deliveries(),
            vec![crate::services::discord::outbound::reaction_control::ReactionControlReplyReason::QueueReactionFailed],
            "failed follow-up requeue reaction must emit exactly one referenced fallback"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uncapped_retry_arms_only_after_terminal_projection_settles_4888() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(100_000_004_888_101);
        let message_id = MessageId::new(100_000_004_888_102);
        let inflight = inflight(channel_id, message_id);

        let outcome = requeue_claude_tui_followup_pre_submit_timeout(
            &shared,
            &provider,
            channel_id,
            &inflight,
            None,
            None,
            "turn-4888-delivery-order",
        )
        .await;
        assert!(outcome.requeued);
        assert!(!outcome.retry_capped);
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "requeue alone must not expose the successor before terminal projection settles"
        );
        let mut completion_events =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
        super::super::super::turn_completion_events::publish_turn_completion_event(
            &shared,
            super::super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
                channel_id,
                Some(message_id.get()),
            ),
        );
        assert!(
            !completion_events
                .try_recv()
                .expect("mailbox release event")
                .queue_is_eligible(),
            "mailbox release must not make the successor queue-eligible"
        );
        let (delivery_tx, delivery_rx) = tokio::sync::oneshot::channel::<()>();
        let boundary = TerminalProjectionSettled::after(async move {
            let _ = delivery_rx.await;
        });
        tokio::pin!(boundary);
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            matches!(
                std::future::Future::poll(boundary.as_mut(), &mut cx),
                std::task::Poll::Pending
            ),
            "the settled token must remain unavailable while the final projection is pending"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "neither direct nor listener kickoff may run before the edit completes"
        );
        delivery_tx.send(()).expect("release final edit");
        let (boundary, ()) = boundary.await;
        let completion_guard = super::guards::CompletionGuard::for_completion_test(
            shared.clone(),
            channel_id,
            message_id.get(),
        );
        assert!(boundary.release_completion_admission(
            &completion_guard,
            Some(outcome),
            &shared,
            &provider,
            channel_id,
            "test_busy_retry_after_completion_postlude_projection",
        ));
        assert!(
            completion_events.try_recv().is_err(),
            "retry scheduling must not fabricate queue admission"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the delivery completion edge arms exactly one successor kickoff"
        );
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id)
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn non_watcher_strict_plan_waits_for_capped_retry_veto_after_projection_4893() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(100_000_004_893_001);
        let message_id = MessageId::new(100_000_004_893_002);
        let key = super::super::super::turn_finalizer::TurnKey::new(
            channel_id,
            message_id.get(),
            shared.restart.current_generation,
        );
        let token = Arc::new(crate::services::provider::CancelToken::new());
        shared
            .mailbox(channel_id)
            .restore_active_turn(token, serenity::model::id::UserId::new(7), message_id)
            .await;
        shared
            .restart
            .global_active
            .store(1, std::sync::atomic::Ordering::Relaxed);
        shared
            .turn_finalizer
            .register_start_with_completion_admission(
                key,
                provider.clone(),
                super::super::super::inflight::RelayOwnerKind::None,
                super::super::super::turn_finalizer::CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                &shared,
            );
        let mut completion_events =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);

        let finalized = shared
            .turn_finalizer
            .submit_terminal(
                key,
                provider.clone(),
                super::super::super::turn_finalizer::TerminalEvent::Complete,
                super::super::super::turn_finalizer::FinalizeContext::bridge(),
                shared.clone(),
            )
            .await;
        assert!(matches!(
            finalized,
            super::super::super::turn_finalizer::FinalizeOutcome::Finalized { .. }
        ));
        let mailbox_release = completion_events
            .try_recv()
            .expect("mailbox release must remain a non-eligible edge for a strict plan");
        assert!(!mailbox_release.queue_is_eligible());
        assert!(completion_events.try_recv().is_err());

        let (projection_tx, projection_rx) = tokio::sync::oneshot::channel::<()>();
        let boundary = TerminalProjectionSettled::after(async move {
            projection_rx.await.expect("release projection boundary");
        });
        tokio::pin!(boundary);
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            std::future::Future::poll(boundary.as_mut(), &mut cx),
            std::task::Poll::Pending
        ));
        assert!(completion_events.try_recv().is_err());

        projection_tx.send(()).expect("settle terminal projection");
        let (boundary, ()) = boundary.await;
        let completion_guard = super::guards::CompletionGuard::for_completion_test(
            shared.clone(),
            channel_id,
            message_id.get(),
        );
        let capped = FollowupRequeueOutcome {
            requeued: true,
            retry_capped: true,
            notice_message_id: message_id,
        };
        assert!(
            !boundary.release_completion_admission(
                &completion_guard,
                Some(capped),
                &shared,
                &provider,
                channel_id,
                "test_non_watcher_strict_plan_capped_retry",
            ),
            "the capped disposition must veto retry scheduling"
        );
        tokio::task::yield_now().await;
        assert!(
            completion_events.try_recv().is_err(),
            "projection must settle before the capped disposition, and its false verdict must permanently veto QueueEligible"
        );
        completion_guard.note_terminal_disposition_settled(true);
        tokio::task::yield_now().await;
        assert!(
            completion_events.try_recv().is_err(),
            "a duplicate allow verdict must not upgrade the first capped-retry veto"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retry_cap_preserves_entry_and_stops_auto_kickoff_4888() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(100_000_004_888_001);
        let message_id = MessageId::new(100_000_004_888_002);
        let inflight = inflight(channel_id, message_id);

        super::super::super::busy_followup_retry_store::bind_notice_if_absent(
            &provider,
            channel_id.get(),
            message_id.get(),
            inflight.current_msg_id,
        )
        .expect("bind busy notice");
        for _ in 1..super::super::super::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT {
            let decision = super::super::super::busy_followup_retry_store::record_busy_retry(
                &provider,
                channel_id.get(),
                message_id.get(),
                inflight.current_msg_id,
            )
            .expect("seed retry budget");
            assert!(!decision.capped);
        }

        let outcome = requeue_claude_tui_followup_pre_submit_timeout(
            &shared,
            &provider,
            channel_id,
            &inflight,
            None,
            None,
            "turn-4888-cap",
        )
        .await;
        assert!(outcome.requeued);
        assert!(outcome.retry_capped);
        let (boundary, ()) = TerminalProjectionSettled::after(async {}).await;
        let completion_guard = super::guards::CompletionGuard::for_completion_test(
            shared.clone(),
            channel_id,
            message_id.get(),
        );
        assert!(
            !boundary.release_completion_admission(
                &completion_guard,
                Some(outcome),
                &shared,
                &provider,
                channel_id,
                "test_capped_busy_retry_after_terminal_projection",
            ),
            "a capped retry must not arm a successor after terminal delivery"
        );
        assert_eq!(
            outcome.notice_message_id,
            MessageId::new(inflight.current_msg_id)
        );
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(snapshot.intervention_queue[0].message_id, message_id);
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the cap-reaching call itself must not schedule an automatic kickoff"
        );
        assert!(
            !shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "the cap-reaching call itself must not register a deferred kickoff task"
        );
        let retry = super::super::super::busy_followup_retry_store::load(
            &provider,
            channel_id.get(),
            message_id.get(),
        )
        .expect("retry state");
        assert_eq!(
            retry.busy_retry_count,
            super::super::super::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT
        );
    }

    /// An idle Claude pane: `status` above the composer box, an empty composer, the bypass footer.
    fn idle_pane(status: &str) -> String {
        let border = "\u{2500}".repeat(60);
        let footer = "  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)";
        format!("\u{23fa} Done.\n\n{status}\n{border}\n\u{276f}\u{a0}\n{border}\n{footer}\n")
    }

    /// The real follow-up submit over a scripted pane; returns its result and the pane writes.
    fn submit_follow_up(
        session: &str,
        pane: &str,
        idle: &std::path::Path,
    ) -> (Result<(), String>, Vec<String>) {
        use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
        let captures = std::iter::repeat_n(pane, 3)
            .chain(["\u{2733} Architecting\u{2026}"])
            .map(|capture| Some(capture.to_string()))
            .collect();
        let spy = SpyGuard::install(SpyState {
            captures,
            ..SpyState::default()
        });
        let submitted = crate::services::claude_tui::input::send_followup_prompt_or_idle_transcript(
            session,
            "queued follow-up",
            None,
            idle,
        );
        let write = |call: &&String| {
            ["keys:", "literal:", "load:", "paste:"]
                .iter()
                .any(|k| call.starts_with(k))
        };
        (
            submitted,
            spy.calls().iter().filter(write).cloned().collect(),
        )
    }

    /// A draft-held follow-up goes through the real classifier into the mailbox, stays whole and
    /// unsent at the cap, and after the person recovers the draft gets one kickoff and one submit.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_follow_up_held_by_a_draft_resumes_once_the_draft_is_recovered() {
        use crate::services::claude_tui::composer_lock::{
            ComposerAdmission, DraftGuard, composer_admission, guard_draft,
        };
        use crate::services::discord::busy_followup_retry_store as retries;
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(100_000_005_845_001);
        let message_id = MessageId::new(100_000_005_845_002);
        let session = format!("draft-hold-{}", uuid::Uuid::new_v4().simple());
        let dir = tempfile::tempdir().expect("transcript dir");
        let idle = dir.path().join("idle.jsonl");
        let turn = r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#;
        std::fs::write(&idle, format!("{turn}\n")).expect("idle transcript");
        guard_draft(&session, DraftGuard::RecoveryRequired);
        let stashed = idle_pane(&format!("{:>58}", "\u{203a} stashed"));
        let (refused, writes) = submit_follow_up(&session, &stashed, &idle);
        let error = refused.expect_err("a held pane takes no follow-up");
        assert_eq!(writes, Vec::<String>::new());

        let classification =
            super::super::streaming_edit_text::classify_raw_tui_error(&provider, &error);
        let runtime = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        let full_response = format!("Error: {error}");
        let base =
            super::super::streaming_edit_text::bridge_claude_tui_followup_busy_readiness_timeout(
                &provider,
                runtime,
                classification,
            ) || bridge_claude_tui_followup_requeue_prompt_error(
                &provider,
                runtime,
                &full_response,
                classification,
            );
        let candidate = claude_tui_followup_requeue_streaming_aware(base, false);
        assert!(candidate, "{error}");

        let mut state = inflight(channel_id, message_id);
        state.tmux_session_name = Some(session.clone());
        state.source_message_ids = vec![message_id.get() - 7, message_id.get()];
        state.followup_pending_uploads = vec!["upload://a.png".into()];
        retries::bind_notice_if_absent(&provider, channel_id.get(), message_id.get(), 9)
            .expect("bind busy notice");
        for _ in 1..retries::MAX_BUSY_RETRY_COUNT {
            retries::record_busy_retry(&provider, channel_id.get(), message_id.get(), 9)
                .expect("seed retry budget");
        }
        let mut outcome = None;
        requeue_if_needed(
            &mut outcome,
            candidate,
            false,
            &shared,
            &provider,
            channel_id,
            &state,
            None,
            None,
            "turn-5845-draft-hold",
        )
        .await;
        let outcome = outcome.expect("requeued");
        assert!(outcome.requeued && outcome.retry_capped);
        let (boundary, ()) = TerminalProjectionSettled::after(async {}).await;
        let completion_guard = super::guards::CompletionGuard::for_completion_test(
            shared.clone(),
            channel_id,
            message_id.get(),
        );
        let reason = "test_draft_hold_cap";
        assert!(!boundary.release_completion_admission(
            &completion_guard,
            Some(outcome),
            &shared,
            &provider,
            channel_id,
            reason,
        ));
        let backlog = || {
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        assert_eq!(backlog(), 0);
        let queued = || async {
            let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
            let entries = snapshot.intervention_queue.iter().map(|entry| {
                let sources = entry.source_message_ids.iter().map(|id| id.get());
                let uploads = entry.pending_uploads.clone();
                (
                    entry.message_id,
                    entry.text.clone(),
                    uploads,
                    sources.collect(),
                )
            });
            entries.collect::<Vec<(_, _, _, Vec<u64>)>>()
        };
        let preserved = vec![(
            message_id,
            "queued follow-up".to_string(),
            state.followup_pending_uploads.clone(),
            vec![message_id.get() - 7, message_id.get()],
        )];
        assert_eq!(queued().await, preserved);
        assert!(retries::is_capped(
            &provider,
            channel_id.get(),
            message_id.get()
        ));

        // The person sent the draft; the next look at the pane sees no stash and an empty composer.
        let settled = idle_pane("");
        let admission = composer_admission(&session, || Some(settled.clone()));
        assert_eq!(admission, ComposerAdmission::Any);
        tokio::time::sleep(DRAFT_RELEASE_POLL + std::time::Duration::from_millis(1)).await;
        assert_eq!(backlog(), 1, "the lifted protection kicks the queue once");
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id)
        );
        assert!(!retries::is_capped(
            &provider,
            channel_id.get(),
            message_id.get()
        ));
        assert_eq!(queued().await, preserved);
        let watch = (channel_id.get(), session.clone());
        assert!(!draft_release_watches().contains(&watch));

        let (resumed, writes) = submit_follow_up(&session, &settled, &idle);
        assert_eq!(resumed, Ok(()));
        assert_eq!(writes, ["literal:queued follow-up", "keys:Enter"]);
        crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
            "claude",
            &session,
            "queued follow-up",
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
