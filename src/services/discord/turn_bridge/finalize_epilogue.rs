//! #3038 (giant-file decompose, registry deadline 2026-08-31): the turn-bridge
//! finalization epilogue — the finalizing-turns counter decrement plus the
//! `has_queued_turns` queue-drain block — moved verbatim out of the tail of
//! `spawn_turn_bridge`'s async body. Behavior-preserving: this is the LAST block
//! of that async body (no borrow-back), so every captured local is threaded in
//! by value with the exact ownership the inline block used (`shared_owned`,
//! `gateway`, `provider`, `request_owner_name` moved; the `Copy` ids/flags
//! passed by value). The only textual change from the original block is the three
//! discord-level `super::` refs deepened to `super::super::` from the child
//! (same seam-fix as `response_delivery.rs`); all other deps reach via
//! `use super::*;`. #3016 single-authority finalizer ledger is untouched — this
//! epilogue runs strictly AFTER the commit and never writes the ledger.

use super::*;

/// Finalization epilogue: decrement the finalizing-turns counters (symmetric
/// with the `fetch_add` at turn start) and, if this turn had queued follow-ups,
/// drain exactly one next turn under the same guards/order as before
/// (`preserve_inflight_for_cleanup_retry` → restart_pending → live-routing
/// validation → dispatch, with the deferred-idle-kickoff fallback when the live
/// Discord context is missing). The queued-turn mailbox side-effects preserve
/// their original order.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_and_drain_queued_turns(
    shared_owned: Arc<SharedData>,
    has_queued_turns: bool,
    preserve_inflight_for_cleanup_retry: bool,
    gateway: Arc<dyn TurnGateway>,
    channel_id: ChannelId,
    provider: ProviderKind,
    request_owner_name: String,
    tmux_last_offset: Option<u64>,
    watcher_owner_channel_id: ChannelId,
) {
    // Finalization complete — decrement counters
    shared_owned
        .restart
        .finalizing_turns
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    shared_owned
        .restart
        .global_finalizing
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    // Note: deferred restart exit is handled by the 5-second poll loop in mod.rs,
    // which saves pending queues before calling check_deferred_restart.
    // Calling it here would risk exiting before other providers save their queues.

    if has_queued_turns {
        // Drain mode: if restart is pending, don't start new turns from queue.
        // The queued messages will be saved to disk and processed after restart.
        if preserve_inflight_for_cleanup_retry {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ QUEUE-GUARD: preserving queued command(s) for channel {} until placeholder cleanup retry commits",
                channel_id
            );
        } else if shared_owned
            .restart
            .restart_pending
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏸ DRAIN: skipping queued turn dequeue for channel {} (restart pending)",
                channel_id
            );
        } else if let Some(bot_owner_provider) = gateway.bot_owner_provider() {
            if let Err(reason) = gateway.validate_live_routing(channel_id).await {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⚠ QUEUE-GUARD: preserving queued command(s) for channel {} (reason={})",
                    channel_id,
                    reason
                );
            } else {
                // The caller's queue flag predates terminal disposition. Select
                // from the current mailbox under the automatic-progression cap
                // policy so a capped retry stays parked while later work can run.
                let next_intervention = match shared_owned
                    .session_transition_lock(channel_id)
                    .try_lock_owned()
                {
                    Ok(transition_guard) => {
                        let outcome = super::super::mailbox_take_next_automatic_intervention(
                            &shared_owned,
                            &bot_owner_provider,
                            channel_id,
                        )
                        .await;
                        drop(transition_guard);
                        outcome
                    }
                    Err(_) => {
                        tracing::debug!(
                            provider = bot_owner_provider.as_str(),
                            channel_id = channel_id.get(),
                            "QUEUE-GUARD: session transition owns channel; preserving queued head"
                        );
                        super::super::arm_slow_idle_queue_backstop_if_queue_nonempty(
                            &shared_owned,
                            &bot_owner_provider,
                            channel_id,
                            "finalize epilogue transition fence",
                        )
                        .await;
                        MailboxTakeNextSoftOutcome::default()
                    }
                };

                if let Some(error) = next_intervention.persistence_error.as_ref() {
                    tracing::error!(
                        provider = bot_owner_provider.as_str(),
                        channel_id = channel_id.get(),
                        error = %error,
                        "QUEUE-GUARD: preserving queued command after pending-queue persistence failure"
                    );
                } else if let Some((intervention, has_more_queued_turns, dispatch_lease)) =
                    next_intervention.into_intervention()
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}] 📋 Processing next queued command");
                    let dispatch_result = gateway
                        .dispatch_queued_turn(
                            channel_id,
                            &intervention,
                            &request_owner_name,
                            has_more_queued_turns,
                            dispatch_lease.clone(),
                        )
                        .await;
                    match dispatch_result {
                        Err(e) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!("  [{ts}]   ⚠ queued command failed: {e}");
                            let requeue = super::super::mailbox_restore_dequeued_head(
                                &shared_owned,
                                &bot_owner_provider,
                                channel_id,
                                intervention,
                                dispatch_lease
                                    .as_ref()
                                    .expect("dequeued intervention must carry its dispatch lease")
                                    .clone(),
                            )
                            .await;
                            if requeue.enqueued {
                                super::super::schedule_deferred_idle_queue_kickoff(
                                    shared_owned.clone(),
                                    bot_owner_provider.clone(),
                                    channel_id,
                                    "requeue-front after dispatch failure (finalize epilogue)",
                                );
                            } else {
                                tracing::error!(
                                    provider = bot_owner_provider.as_str(),
                                    channel_id = channel_id.get(),
                                    refusal_reason = requeue
                                        .refusal_reason
                                        .map(|reason| reason.as_str())
                                        .unwrap_or("none"),
                                    persistence_error =
                                        requeue.persistence_error.as_deref().unwrap_or("none"),
                                    "queued command dispatch failed and dequeued-head restore was rejected"
                                );
                            }
                        }
                        Ok(()) => {
                            super::super::mailbox_abandon_unclaimed_dispatch_after_success(
                                &shared_owned,
                                &bot_owner_provider,
                                channel_id,
                                intervention.message_id,
                                dispatch_lease
                                    .as_ref()
                                    .expect("dequeued intervention must carry its dispatch lease")
                                    .clone(),
                            )
                            .await;
                        }
                    }
                    drop(dispatch_lease);
                }
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📦 preserving queued command(s): missing live Discord context — scheduling deferred drain"
            );
            if let Some(offset) = tmux_last_offset
                && let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
            {
                if let Ok(mut guard) = watcher.resume_offset.lock() {
                    *guard = Some(offset);
                }
                watcher.paused.store(false, Ordering::Relaxed);
            }
            super::super::schedule_deferred_idle_queue_kickoff(
                shared_owned.clone(),
                provider.clone(),
                channel_id,
                "turn bridge queued backlog",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    type TestGatewayFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

    struct FailingQueuedDispatchGateway;

    struct RecordingFailingQueuedDispatchGateway {
        dispatched_message_ids: Arc<std::sync::Mutex<Vec<MessageId>>>,
    }

    impl TurnGateway for FailingQueuedDispatchGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async { Ok(MessageId::new(1_500_000_000_001_001)) })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> TestGatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
            _dispatch_lease: Option<
                std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
            >,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Err("forced dispatch failure".to_string()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            true
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Claude)
        }
    }

    impl TurnGateway for RecordingFailingQueuedDispatchGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async { Ok(MessageId::new(1_500_000_000_001_002)) })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> TestGatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            intervention: &'a Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
            _dispatch_lease: Option<
                std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
            >,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            let dispatched_message_ids = self.dispatched_message_ids.clone();
            let message_id = intervention.message_id;
            Box::pin(async move {
                dispatched_message_ids
                    .lock()
                    .expect("dispatch record lock")
                    .push(message_id);
                Err("forced dispatch failure".to_string())
            })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            true
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Claude)
        }
    }

    fn queued_intervention(message_id: u64) -> Intervention {
        Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "queued dispatch failure".to_string(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[test]
    fn session_transition_fences_finalizer_dequeue_and_preserves_fifo_4794() {
        let tmp = tempfile::tempdir().expect("runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(tmp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                shared.restart.finalizing_turns.store(2, Ordering::Relaxed);
                shared.restart.global_finalizing.store(2, Ordering::Relaxed);
                let provider = ProviderKind::Claude;
                let channel_id = ChannelId::new(4_794_300);
                let first = queued_intervention(4_794_301);
                let second = queued_intervention(4_794_302);
                let dispatched_message_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
                shared
                    .mailbox(channel_id)
                    .replace_queue(
                        vec![first.clone(), second.clone()],
                        super::super::queue_persistence_context(&shared, &provider, channel_id),
                    )
                    .await;

                let transition_guard = shared
                    .session_transition_lock(channel_id)
                    .lock_owned()
                    .await;
                finalize_and_drain_queued_turns(
                    shared.clone(),
                    true,
                    false,
                    Arc::new(RecordingFailingQueuedDispatchGateway {
                        dispatched_message_ids: dispatched_message_ids.clone(),
                    }),
                    channel_id,
                    provider.clone(),
                    "requester".to_string(),
                    None,
                    channel_id,
                )
                .await;
                let blocked_snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
                assert_eq!(
                    blocked_snapshot
                        .intervention_queue
                        .iter()
                        .map(|item| item.message_id)
                        .collect::<Vec<_>>(),
                    vec![first.message_id, second.message_id],
                    "a finalizer running under /resume transition ownership must not dequeue the queued head"
                );
                assert_eq!(blocked_snapshot.pending_user_dispatch, None);
                assert_eq!(
                    *dispatched_message_ids.lock().expect("dispatch record lock"),
                    Vec::<MessageId>::new(),
                    "transition ownership must suppress finalizer dispatch"
                );

                drop(transition_guard);
                finalize_and_drain_queued_turns(
                    shared.clone(),
                    true,
                    false,
                    Arc::new(RecordingFailingQueuedDispatchGateway {
                        dispatched_message_ids: dispatched_message_ids.clone(),
                    }),
                    channel_id,
                    provider,
                    "requester".to_string(),
                    None,
                    channel_id,
                )
                .await;
                assert_eq!(
                    *dispatched_message_ids.lock().expect("dispatch record lock"),
                    vec![first.message_id],
                    "after transition release the original head A must dispatch first"
                );
                let released_snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
                assert_eq!(
                    released_snapshot
                        .intervention_queue
                        .iter()
                        .map(|item| item.message_id)
                        .collect::<Vec<_>>(),
                    vec![first.message_id, second.message_id],
                    "after transition release the original head A must dequeue first; forced dispatch failure restores A ahead of B"
                );
                assert_eq!(released_snapshot.pending_user_dispatch, None);
            });
    }

    #[test]
    fn stale_queue_flag_skips_capped_retry_and_dispatches_unrelated_backlog_4893() {
        let tmp = tempfile::tempdir().expect("runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(tmp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                shared.restart.finalizing_turns.store(1, Ordering::Relaxed);
                shared.restart.global_finalizing.store(1, Ordering::Relaxed);
                let provider = ProviderKind::Claude;
                let channel_id = ChannelId::new(4_893_320);
                let capped = queued_intervention(4_893_321);
                let unrelated = queued_intervention(4_893_322);
                let notice_id = MessageId::new(4_893_323);
                super::super::busy_followup_retry_store::bind_notice_if_absent(
                    &provider,
                    channel_id.get(),
                    capped.message_id.get(),
                    notice_id.get(),
                )
                .expect("bind capped retry");
                for _ in 0..super::super::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT {
                    super::super::busy_followup_retry_store::record_busy_retry(
                        &provider,
                        channel_id.get(),
                        capped.message_id.get(),
                        notice_id.get(),
                    )
                    .expect("record capped retry");
                }
                shared
                    .mailbox(channel_id)
                    .replace_queue(
                        vec![capped.clone(), unrelated.clone()],
                        super::super::queue_persistence_context(&shared, &provider, channel_id),
                    )
                    .await;

                finalize_and_drain_queued_turns(
                    shared.clone(),
                    true,
                    false,
                    Arc::new(FailingQueuedDispatchGateway),
                    channel_id,
                    provider,
                    "requester".to_string(),
                    None,
                    channel_id,
                )
                .await;

                let snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
                assert_eq!(
                    snapshot
                        .intervention_queue
                        .iter()
                        .map(|item| item.message_id)
                        .collect::<Vec<_>>(),
                    vec![unrelated.message_id, capped.message_id],
                    "the stale epilogue flag must select unrelated B; its forced dispatch failure restores B without dispatching capped A"
                );
            });
    }

    #[test]
    fn dispatch_failure_requeue_front_schedules_deferred_kickoff() {
        let tmp = tempfile::tempdir().expect("runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(tmp.path());

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = crate::services::discord::make_shared_data_for_tests();
                shared.restart.finalizing_turns.store(1, Ordering::Relaxed);
                shared.restart.global_finalizing.store(1, Ordering::Relaxed);
                let provider = ProviderKind::Claude;
                let channel_id = ChannelId::new(4_024_280);
                let intervention = queued_intervention(4_024_281);
                shared
                    .mailbox(channel_id)
                    .replace_queue(
                        vec![intervention.clone()],
                        super::super::queue_persistence_context(&shared, &provider, channel_id),
                    )
                    .await;

                finalize_and_drain_queued_turns(
                    shared.clone(),
                    true,
                    false,
                    Arc::new(FailingQueuedDispatchGateway),
                    channel_id,
                    provider,
                    "requester".to_string(),
                    None,
                    channel_id,
                )
                .await;

                let snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
                assert_eq!(snapshot.intervention_queue.len(), 1);
                assert_eq!(
                    snapshot.intervention_queue[0].message_id,
                    intervention.message_id
                );
                assert_eq!(
                    shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
                    1,
                    "requeue-front after dispatch failure must re-arm the drain"
                );
            });
    }
}
