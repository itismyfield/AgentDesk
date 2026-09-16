use super::*;
use crate::services::discord::turn_completion_events::{self, TurnCompletionPhase};
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::provider::CancelToken;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use serenity::model::id::UserId;
use std::sync::atomic::Ordering;

const PROVIDER: ProviderKind = ProviderKind::Codex;

#[tokio::test]
async fn committed_a_recovery_after_operator_release_cannot_cancel_claimed_b_r3() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575430);
        let original = seed(&shared, channel).await;
        let mut old_row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        old_row.full_response = "committed A".into();
        old_row.response_sent_offset = old_row.full_response.len();
        old_row.terminal_delivery_committed = true;
        inflight::save_inflight_state(&old_row).unwrap();
        assert_eq!(
            release_on(&shared, &PROVIDER, channel, original)
                .await
                .unwrap()["released"],
            true
        );
        let successor = seed(&shared, channel).await;
        let token = shared
            .mailbox(channel)
            .snapshot()
            .await
            .cancel_token
            .unwrap();
        // The real intake path claims B before awaited bootstrap registers Start.
        let mut events = turn_completion_events::subscribe_turn_completion_events(&shared);
        assert!(
            !super::super::recovery::reregister_active_turn_from_inflight(&shared, &old_row).await
        );
        assert!(
            !token.cancelled.load(Ordering::Acquire),
            "old committed A recovery must not cancel claimed B before Start"
        );
        let live = shared
            .mailbox(channel)
            .snapshot()
            .await
            .cancel_token
            .unwrap();
        assert!(Arc::ptr_eq(&live, &token));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert_eq!(
            inflight::load_inflight_state(&PROVIDER, channel.get())
                .unwrap()
                .turn_nonce,
            Some(successor.expected.turn_nonce)
        );
        assert!(events.try_recv().is_err());
    })
    .await;
}

#[tokio::test]
async fn successor_normal_complete_preserves_negative_admission_after_operator_release_r2() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575405);
        let original = seed(&shared, channel).await;
        release_on(&shared, &PROVIDER, channel, original)
            .await
            .unwrap();
        let successor = seed(&shared, channel).await;
        let key = TurnKey::new(channel, 123, successor.expected.generation)
            .with_episode_nonce(Some(&successor.expected.turn_nonce));
        shared
            .turn_finalizer
            .register_start_with_completion_admission(
                key,
                PROVIDER,
                inflight::RelayOwnerKind::Watcher,
                CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                &shared,
            );
        shared
            .turn_finalizer
            .note_terminal_projection_settled(key, false, shared.clone());
        shared
            .turn_finalizer
            .note_terminal_disposition_settled(key, false, shared.clone());
        let row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        let mut events = turn_completion_events::subscribe_turn_completion_events(&shared);
        let outcome = shared
            .turn_finalizer
            .submit_terminal_with_claim_snapshot(
                key,
                PROVIDER,
                TerminalEvent::Complete,
                FinalizeContext::bridge(),
                Some(SyntheticClaimSnapshot::from_row(&row)),
                shared.clone(),
            )
            .await;
        assert!(matches!(
            outcome,
            FinalizeOutcome::Finalized {
                removed_token: Some(_),
                ..
            }
        ));
        let mut released = false;
        while let Ok(event) = events.try_recv() {
            assert_ne!(
                event.phase,
                TurnCompletionPhase::QueueEligible,
                "ordinary B completion cannot bypass B's negative delivery/disposition evidence"
            );
            released |= event.phase == TurnCompletionPhase::MailboxReleased;
        }
        assert!(
            released,
            "ordinary B completion still releases its own mailbox"
        );
    })
    .await;
}

async fn seed(shared: &Arc<SharedData>, channel: ChannelId) -> ReleaseRequest {
    let mailbox = shared.mailbox(channel);
    let token = Arc::new(CancelToken::new());
    mailbox
        .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(123))
        .await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let mut row = inflight::InflightTurnState::new(
        PROVIDER,
        channel.get(),
        None,
        7,
        123,
        456,
        "prompt".into(),
        Some("provider-session-kept".into()),
        Some("tmux-kept".into()),
        None,
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_owned);
    inflight::save_inflight_state(&row).unwrap();
    ReleaseRequest {
        expected: identity(shared, &PROVIDER, channel).await.unwrap().unwrap(),
        reason: "operator verified completed provider turn".into(),
    }
}

#[tokio::test]
async fn operator_release_preserves_queue_and_provider_and_is_idempotent() {
    with_isolated_runtime_root(|| async {
        for (channel_id, finalized_lease) in [(575401, false), (575411, true)] {
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
            let channel = ChannelId::new(channel_id);
            let mailbox = shared.mailbox(channel);
            let mut request = seed(&shared, channel).await;
            if finalized_lease {
                let row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
                let key = TurnKey::new(channel, 123, request.expected.generation)
                    .with_episode_nonce(Some(&request.expected.turn_nonce));
                shared
                    .turn_finalizer
                    .submit_terminal(
                        key,
                        PROVIDER,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        shared.clone(),
                    )
                    .await;
                let restored = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
                    request.expected.turn_nonce.clone(),
                )));
                mailbox
                    .restore_active_turn(restored, UserId::new(7), MessageId::new(123))
                    .await;
                inflight::save_inflight_state(&row).unwrap();
                shared.restart.global_active.store(1, Ordering::Relaxed);
                request.expected = identity(&shared, &PROVIDER, channel)
                    .await
                    .unwrap()
                    .unwrap();
            }
            let token = mailbox.snapshot().await.cancel_token.unwrap();
            for id in [124, 125] {
                let queued = Intervention {
                    author_id: UserId::new(7),
                    author_is_bot: false,
                    message_id: MessageId::new(id),
                    queued_generation: shared.restart.current_generation,
                    source_message_ids: vec![MessageId::new(id)],
                    source_message_queued_generations: vec![],
                    source_text_segments: vec![],
                    text: format!("queued {id}"),
                    mode: InterventionMode::Soft,
                    created_at: Instant::now(),
                    reply_context: None,
                    has_reply_boundary: false,
                    merge_consecutive: false,
                    pending_uploads: vec![],
                    voice_announcement: None,
                };
                assert!(
                    mailbox
                        .enqueue(
                            queued,
                            super::super::queue_persistence_context(&shared, &PROVIDER, channel)
                        )
                        .await
                        .enqueued
                );
            }
            let before = mailbox.snapshot().await;
            let key = TurnKey::new(channel, 123, request.expected.generation)
                .with_episode_nonce(Some(&request.expected.turn_nonce));
            shared
                .turn_finalizer
                .register_start_with_completion_admission(
                    key,
                    PROVIDER,
                    inflight::RelayOwnerKind::Watcher,
                    CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                    &shared,
                );
            shared
                .turn_finalizer
                .note_terminal_projection_settled(key, false, shared.clone());
            shared
                .turn_finalizer
                .note_terminal_disposition_settled(key, false, shared.clone());
            let mut rx = turn_completion_events::subscribe_turn_completion_events(&shared);
            let result = release_on(&shared, &PROVIDER, channel, request.clone())
                .await
                .unwrap();
            assert_eq!(result["released"], true);
            let after = mailbox.snapshot().await;
            assert!(after.cancel_token.is_none());
            assert_eq!(
                after
                    .intervention_queue
                    .iter()
                    .map(|q| (q.message_id, &q.text))
                    .collect::<Vec<_>>(),
                before
                    .intervention_queue
                    .iter()
                    .map(|q| (q.message_id, &q.text))
                    .collect::<Vec<_>>()
            );
            assert_eq!(after.pending_user_dispatch, before.pending_user_dispatch);
            assert!(
                !token.cancelled.load(Ordering::Acquire),
                "provider must never receive cancellation"
            );
            assert!(
                token.is_completion_cleanup(),
                "retire watchdog without provider abort"
            );
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
            assert!(inflight::load_inflight_state(&PROVIDER, channel.get()).is_none());
            assert_eq!(
                rx.try_recv().unwrap().phase,
                TurnCompletionPhase::MailboxReleased
            );
            assert_eq!(
                rx.try_recv().unwrap().phase,
                TurnCompletionPhase::QueueEligible
            );
            assert_eq!(
                release_on(&shared, &PROVIDER, channel, request)
                    .await
                    .unwrap()["released"],
                false
            );
            assert!(rx.try_recv().is_err());
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        }
    })
    .await;
}

#[tokio::test]
async fn operator_release_rejects_changed_identity_or_generation_without_effects() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575402);
        let mailbox = shared.mailbox(channel);
        let request = seed(&shared, channel).await;
        let token = mailbox.snapshot().await.cancel_token.unwrap();
        let mut rx = turn_completion_events::subscribe_turn_completion_events(&shared);
        for axis in 0..3 {
            let mut stale = request.clone();
            match axis {
                0 => stale.expected.generation += 1,
                1 => stale.expected.turn_nonce.push('x'),
                _ => stale.expected.user_message_id += 1,
            }
            assert!(
                release_on(&shared, &PROVIDER, channel, stale)
                    .await
                    .is_err()
            );
        }
        assert!(Arc::ptr_eq(
            &token,
            &mailbox.snapshot().await.cancel_token.unwrap()
        ));
        assert!(!token.is_completion_cleanup());
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(inflight::load_inflight_state(&PROVIDER, channel.get()).is_some());
        assert!(rx.try_recv().is_err());
        // A row that is present but names a DIFFERENT episode is still a
        // refusal after #5951 S2 — only an ABSENT row opens the rowless lane.
        let mut foreign = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        foreign.turn_nonce = Some(format!("{}-successor", request.expected.turn_nonce));
        inflight::save_inflight_state(&foreign).unwrap();
        let error = release_on(&shared, &PROVIDER, channel, request)
            .await
            .unwrap_err();
        assert!(error.contains("differs from this episode"), "{error}");
        assert!(Arc::ptr_eq(
            &token,
            &mailbox.snapshot().await.cancel_token.unwrap()
        ));
        assert!(!token.is_completion_cleanup());
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_err());
    })
    .await;
}

#[tokio::test]
async fn operator_release_cas_then_successor_keeps_new_turn_and_row() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575403);
        let mailbox = shared.mailbox(channel);
        let request = seed(&shared, channel).await;
        let key = TurnKey::new(
            channel,
            request.expected.user_message_id,
            request.expected.generation,
        )
        .with_episode_nonce(Some(&request.expected.turn_nonce));
        let operator = OperatorRelease {
            request: request.clone(),
            observed_before: Instant::now(),
            clear_outcome: Default::default(),
        };
        let finish = operator.claim(&shared, &PROVIDER, key).await.unwrap();
        let successor = seed(&shared, channel).await;
        let successor_key = TurnKey::new(channel, 123, successor.expected.generation)
            .with_episode_nonce(Some(&successor.expected.turn_nonce));
        shared.restart.global_active.store(2, Ordering::Relaxed);
        let successor_token = mailbox.snapshot().await.cancel_token.unwrap();
        let event = TerminalEvent::OperatorRelease(Box::new(operator));
        super::super::turn_finalizer::do_finalize_with_release_for_test(
            key,
            PROVIDER,
            &event,
            FinalizeContext::bridge(),
            None,
            &shared,
            Some(finish),
        )
        .await;
        assert_eq!(
            identity(&shared, &PROVIDER, channel).await.unwrap(),
            Some(successor.expected)
        );
        assert!(!successor_token.is_completion_cleanup());
        assert!(!successor_token.cancelled.load(Ordering::Acquire));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert_eq!(
            inflight::load_inflight_state(&PROVIDER, channel.get())
                .unwrap()
                .turn_nonce
                .as_deref(),
            successor_token.turn_nonce()
        );
        assert!(
            release_on(&shared, &PROVIDER, channel, request)
                .await
                .is_err()
        );
        shared.turn_finalizer.register_start(
            successor_key,
            PROVIDER,
            inflight::RelayOwnerKind::Watcher,
            &shared,
        );
        shared
            .turn_finalizer
            .submit_terminal(
                key,
                PROVIDER,
                TerminalEvent::Complete,
                FinalizeContext::bridge(),
                shared.clone(),
            )
            .await;
        assert!(
            !successor_token.cancelled.load(Ordering::Acquire),
            "late A terminal cannot cancel B"
        );
        let row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        let captured = SyntheticClaimSnapshot::from_row(&row);
        assert_eq!(
            inflight::clear_inflight_state_for_captured_episode(
                &PROVIDER,
                channel.get(),
                &inflight::InflightTurnIdentity::from_state(&row),
                successor_token.turn_nonce()
            ),
            inflight::GuardedClearOutcome::Cleared
        );
        let completed = shared
            .turn_finalizer
            .submit_terminal_with_claim_snapshot(
                successor_key,
                PROVIDER,
                TerminalEvent::Complete,
                FinalizeContext::bridge(),
                Some(captured),
                shared.clone(),
            )
            .await;
        assert!(
            matches!(
                completed,
                FinalizeOutcome::Finalized {
                    removed_token: Some(_),
                    ..
                }
            ),
            "B completes normally using its captured nonce"
        );
        assert!(mailbox.snapshot().await.cancel_token.is_none());
    })
    .await;
}

#[tokio::test]
async fn operator_release_strict_clear_preserves_nonce_less_successor() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575404);
        let mailbox = shared.mailbox(channel);
        let request = seed(&shared, channel).await;
        let mut row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        let expected = inflight::InflightTurnIdentity::from_state(&row);
        row.turn_nonce = None;
        inflight::save_inflight_state(&row).unwrap();
        assert_eq!(
            inflight::clear_inflight_state_for_captured_episode(
                &PROVIDER,
                channel.get(),
                &expected,
                Some(&request.expected.turn_nonce)
            ),
            inflight::GuardedClearOutcome::UserMsgMismatch
        );
        assert!(inflight::load_inflight_state(&PROVIDER, channel.get()).is_some());
    })
    .await;
}

/// #5951 S2 closing test 2 — with no durable row at all the operator release
/// COMMITS: the mailbox CAS is the only authority it ever needed, the provider
/// is never cancelled, and the second call is still idempotent. Before S2 the
/// required-row gate turned this channel into a permanently stuck lease.
#[tokio::test]
async fn operator_release_commits_without_any_inflight_row() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575406);
        let mailbox = shared.mailbox(channel);
        let request = seed(&shared, channel).await;
        let token = mailbox.snapshot().await.cancel_token.unwrap();
        let row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        assert_eq!(
            inflight::clear_inflight_state_for_captured_episode(
                &PROVIDER,
                channel.get(),
                &inflight::InflightTurnIdentity::from_state(&row),
                token.turn_nonce(),
            ),
            inflight::GuardedClearOutcome::Cleared
        );
        assert!(
            inflight::load_inflight_state(&PROVIDER, channel.get()).is_none(),
            "precondition: the lease outlived its projection"
        );
        let mut rx = turn_completion_events::subscribe_turn_completion_events(&shared);
        let result = release_on(&shared, &PROVIDER, channel, request.clone())
            .await
            .unwrap();
        assert_eq!(result["released"], true);
        assert_eq!(result["status"], "operator_released");
        assert!(mailbox.snapshot().await.cancel_token.is_none());
        assert!(
            !token.cancelled.load(Ordering::Acquire),
            "provider must never receive cancellation"
        );
        assert!(token.is_completion_cleanup());
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(inflight::load_inflight_state(&PROVIDER, channel.get()).is_none());
        assert_eq!(
            rx.try_recv().unwrap().phase,
            TurnCompletionPhase::MailboxReleased
        );
        assert_eq!(
            release_on(&shared, &PROVIDER, channel, request)
                .await
                .unwrap()["released"],
            false
        );
    })
    .await;
}

/// #5951 S2 closing test 3 — a recovery / TUI-direct episode binds no user
/// message id. Before S2 `identity` refused it outright, so it was invisible.
/// It is now INSPECTABLE as id 0; release is refused with a named reason
/// because `release_turn_lease_if_matches` keys on an exact `MessageId` and
/// `MessageId::new(0)` panics — the CAS is deliberately left unchanged.
#[tokio::test]
async fn unbound_message_id_lease_is_inspectable_and_refuses_release_without_effects() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575407);
        let mailbox = shared.mailbox(channel);
        let token = Arc::new(CancelToken::new());
        mailbox
            .recovery_kickoff(token.clone(), UserId::new(7), None)
            .await;
        shared.restart.global_active.store(1, Ordering::Relaxed);
        assert!(
            mailbox.snapshot().await.active_user_message_id.is_none(),
            "precondition: this episode never bound a user message"
        );
        let current = identity(&shared, &PROVIDER, channel)
            .await
            .expect("an unbound episode is inspectable")
            .expect("the lease exists");
        assert_eq!(current.user_message_id, 0);
        assert_eq!(Some(current.turn_nonce.as_str()), token.turn_nonce());
        let mut rx = turn_completion_events::subscribe_turn_completion_events(&shared);
        let error = release_on(
            &shared,
            &PROVIDER,
            channel,
            ReleaseRequest {
                expected: current,
                reason: "operator verified the provider finished".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("no bound message identity"), "{error}");
        assert!(mailbox.snapshot().await.cancel_token.is_some());
        assert!(!token.is_completion_cleanup());
        assert!(!token.cancelled.load(Ordering::Acquire));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_err());
    })
    .await;
}

/// #5951 S2 closing test 4 — opening the rowless lane must not unpin authority.
/// A `restart_mode` row marks a PLANNED restart that owns this episode, so both
/// inspect and release stay refusals and the row is left byte-identical.
#[tokio::test]
async fn restart_mode_row_still_refuses_operator_inspect_and_release() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575408);
        let mailbox = shared.mailbox(channel);
        let request = seed(&shared, channel).await;
        let token = mailbox.snapshot().await.cancel_token.unwrap();
        let mut row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        row.set_restart_mode(crate::services::discord::InflightRestartMode::HotSwapHandoff);
        inflight::save_inflight_state(&row).unwrap();
        let before = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        assert!(before.restart_mode.is_some());
        let inspect_error = matching_inflight(&PROVIDER, &request.expected).unwrap_err();
        assert!(
            inspect_error.contains("protected by a planned restart or a rebind origin"),
            "{inspect_error}"
        );
        let mut rx = turn_completion_events::subscribe_turn_completion_events(&shared);
        let error = release_on(&shared, &PROVIDER, channel, request.clone())
            .await
            .unwrap_err();
        assert!(
            error.contains("protected by a planned restart or a rebind origin"),
            "{error}"
        );
        assert!(Arc::ptr_eq(
            &token,
            &mailbox.snapshot().await.cancel_token.unwrap()
        ));
        assert!(!token.is_completion_cleanup());
        assert!(!token.cancelled.load(Ordering::Acquire));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        let after = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        assert_eq!(after.restart_mode, before.restart_mode);
        assert_eq!(after.turn_nonce, before.turn_nonce);
        assert!(rx.try_recv().is_err());
        // The direct claim path is pinned too, not only the HTTP entry point.
        let key = TurnKey::new(
            channel,
            request.expected.user_message_id,
            request.expected.generation,
        )
        .with_episode_nonce(Some(&request.expected.turn_nonce));
        let operator = OperatorRelease {
            request,
            observed_before: Instant::now(),
            clear_outcome: Default::default(),
        };
        assert!(operator.claim(&shared, &PROVIDER, key).await.is_none());
        assert!(mailbox.snapshot().await.cancel_token.is_some());
    })
    .await;
}

/// #5951 S2 closing test 5 — same pin for a `rebind_origin` row: the rebind
/// owner holds this episode, so the rowless lane must not hand it to an
/// operator and the row must survive the attempt untouched.
#[tokio::test]
async fn rebind_origin_row_still_refuses_operator_inspect_and_release() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575409);
        let mailbox = shared.mailbox(channel);
        let request = seed(&shared, channel).await;
        let token = mailbox.snapshot().await.cancel_token.unwrap();
        let mut row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        row.rebind_origin = true;
        inflight::save_inflight_state(&row).unwrap();
        let before = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        assert!(before.rebind_origin);
        let inspect_error = matching_inflight(&PROVIDER, &request.expected).unwrap_err();
        assert!(
            inspect_error.contains("protected by a planned restart or a rebind origin"),
            "{inspect_error}"
        );
        let mut rx = turn_completion_events::subscribe_turn_completion_events(&shared);
        let error = release_on(&shared, &PROVIDER, channel, request.clone())
            .await
            .unwrap_err();
        assert!(
            error.contains("protected by a planned restart or a rebind origin"),
            "{error}"
        );
        assert!(Arc::ptr_eq(
            &token,
            &mailbox.snapshot().await.cancel_token.unwrap()
        ));
        assert!(!token.is_completion_cleanup());
        assert!(!token.cancelled.load(Ordering::Acquire));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        let after = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        assert!(after.rebind_origin);
        assert_eq!(after.turn_nonce, before.turn_nonce);
        assert!(rx.try_recv().is_err());
        let key = TurnKey::new(
            channel,
            request.expected.user_message_id,
            request.expected.generation,
        )
        .with_episode_nonce(Some(&request.expected.turn_nonce));
        let operator = OperatorRelease {
            request,
            observed_before: Instant::now(),
            clear_outcome: Default::default(),
        };
        assert!(operator.claim(&shared, &PROVIDER, key).await.is_none());
        assert!(mailbox.snapshot().await.cancel_token.is_some());
    })
    .await;
}

/// #5951 r2 P1-1 — the rowless twin of the two pin tests above. `restart_mode`
/// lives on the live `CancelToken` and the row only carries a copy, so an
/// episode whose projection is gone is STILL pinned. Before this fix the
/// rowless lane checked the row alone and released a DrainRestart episode.
#[tokio::test]
async fn restart_mode_pinned_token_still_refuses_release_without_any_row() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575410);
        let mailbox = shared.mailbox(channel);
        let request = seed(&shared, channel).await;
        let token = mailbox.snapshot().await.cancel_token.unwrap();
        let row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        inflight::clear_inflight_state_for_captured_episode(
            &PROVIDER,
            channel.get(),
            &inflight::InflightTurnIdentity::from_state(&row),
            token.turn_nonce(),
        );
        assert!(
            inflight::load_inflight_state_read_only(&PROVIDER, channel.get()).is_none(),
            "precondition: the projection is gone, so only the token carries the pin"
        );
        token.set_restart_mode(Some(
            crate::services::discord::InflightRestartMode::DrainRestart,
        ));
        let mut rx = turn_completion_events::subscribe_turn_completion_events(&shared);
        let inspect_error = identity(&shared, &PROVIDER, channel).await.unwrap_err();
        assert!(
            inspect_error.contains("pinned by a planned restart"),
            "{inspect_error}"
        );
        let error = release_on(&shared, &PROVIDER, channel, request.clone())
            .await
            .unwrap_err();
        assert!(error.contains("pinned by a planned restart"), "{error}");
        assert!(Arc::ptr_eq(
            &token,
            &mailbox.snapshot().await.cancel_token.unwrap()
        ));
        assert!(!token.is_completion_cleanup());
        assert!(!token.cancelled.load(Ordering::Acquire));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_err());
        let key = TurnKey::new(
            channel,
            request.expected.user_message_id,
            request.expected.generation,
        )
        .with_episode_nonce(Some(&request.expected.turn_nonce));
        let operator = OperatorRelease {
            request,
            observed_before: Instant::now(),
            clear_outcome: Default::default(),
        };
        assert!(operator.claim(&shared, &PROVIDER, key).await.is_none());
        assert!(mailbox.snapshot().await.cancel_token.is_some());
    })
    .await;
}

/// #5951 r2 P1-2 — an unbound (id 0) episode that DOES have its own row is
/// inspectable. Its row stores `user_msg_id = 0` while
/// `effective_finalizer_turn_id()` synthesises a non-zero id, so an
/// unconditional id comparison rejected the episode's own row and reported it
/// as foreign. The nonce axis still rejects a genuinely foreign row.
#[tokio::test]
async fn unbound_message_id_lease_with_its_own_row_is_inspectable() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575411);
        let mailbox = shared.mailbox(channel);
        let token = Arc::new(CancelToken::new());
        mailbox
            .recovery_kickoff(token.clone(), UserId::new(7), None)
            .await;
        let mut row = inflight::InflightTurnState::new(
            PROVIDER,
            channel.get(),
            None,
            7,
            0,
            456,
            "prompt".into(),
            Some("provider-session-kept".into()),
            Some("tmux-kept".into()),
            None,
            None,
            0,
        );
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        inflight::save_inflight_state(&row).unwrap();
        let stored = inflight::load_inflight_state_read_only(&PROVIDER, channel.get()).unwrap();
        assert_eq!(stored.user_msg_id, 0);
        assert_ne!(
            stored.effective_finalizer_turn_id(),
            0,
            "the row synthesises a non-zero finalizer id: that is why the id axis must be skipped"
        );
        let current = identity(&shared, &PROVIDER, channel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.user_message_id, 0);
        assert!(
            matching_inflight(&PROVIDER, &current).unwrap().is_some(),
            "the episode's own row must not read as foreign"
        );
        let mut foreign =
            inflight::load_inflight_state_read_only(&PROVIDER, channel.get()).unwrap();
        foreign.turn_nonce = Some(format!("{}-successor", current.turn_nonce));
        inflight::save_inflight_state(&foreign).unwrap();
        let error = matching_inflight(&PROVIDER, &current).unwrap_err();
        assert!(error.contains("differs from this episode"), "{error}");
    })
    .await;
}

/// #5951 r2 P2-1 — inspect is advertised as a non-mutating probe, so its row
/// read must not take the loader that rewrites the sidecar under a lock to
/// backfill `finalizer_turn_id`.
#[tokio::test]
async fn inspect_row_read_leaves_the_sidecar_bytes_unchanged() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575412);
        let request = seed(&shared, channel).await;
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().expect("isolated runtime root"),
            &PROVIDER,
            channel.get(),
        );
        let mut parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // `ensure_finalizer_turn_id` backfills whenever the stored id differs
        // from the effective one, so a stored 0 deterministically arms the
        // rewrite that a mutating read would perform.
        parsed
            .as_object_mut()
            .unwrap()
            .insert("finalizer_turn_id".into(), serde_json::json!(0));
        std::fs::write(&path, serde_json::to_vec(&parsed).unwrap()).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            matching_inflight(&PROVIDER, &request.expected)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the inspect row read must not rewrite the sidecar"
        );
    })
    .await;
}

/// #5951 r2 P2-2 — `release_on` refuses id 0 upstream, so this guard only
/// protects a DIRECT `claim` caller. Exercise that entry: without the guard
/// `MessageId::new(0)` panics rather than returning a refusal.
#[tokio::test]
async fn claim_refuses_unbound_message_id_without_panic() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575413);
        let mailbox = shared.mailbox(channel);
        let token = Arc::new(CancelToken::new());
        mailbox
            .recovery_kickoff(token.clone(), UserId::new(7), None)
            .await;
        let expected = identity(&shared, &PROVIDER, channel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(expected.user_message_id, 0);
        let key = TurnKey::new(channel, 0, expected.generation)
            .with_episode_nonce(Some(&expected.turn_nonce));
        let operator = OperatorRelease {
            request: ReleaseRequest {
                expected,
                reason: "operator verified the provider finished".into(),
            },
            observed_before: Instant::now(),
            clear_outcome: Default::default(),
        };
        assert!(operator.claim(&shared, &PROVIDER, key).await.is_none());
        assert!(mailbox.snapshot().await.cancel_token.is_some());
        assert!(!token.is_completion_cleanup());
    })
    .await;
}

/// #5951 r2 P2-3 — a row appearing between the rowless check and the mailbox
/// CAS is this episode's own late projection, so it is swept; a successor's row
/// that appeared in the same window carries a different nonce and survives.
#[tokio::test]
async fn late_projection_sweep_clears_only_this_episodes_row() {
    with_isolated_runtime_root(|| async {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(575414);
        let request = seed(&shared, channel).await;
        let nonce = request.expected.turn_nonce.clone();
        assert_eq!(
            OperatorRelease::clear_late_projection(&PROVIDER, channel, &nonce),
            inflight::GuardedClearOutcome::Cleared
        );
        assert!(inflight::load_inflight_state_read_only(&PROVIDER, channel.get()).is_none());
        assert_eq!(
            OperatorRelease::clear_late_projection(&PROVIDER, channel, &nonce),
            inflight::GuardedClearOutcome::Missing,
            "no row at all is reported as Missing, not as a clear"
        );
        let mut successor = inflight::InflightTurnState::new(
            PROVIDER,
            channel.get(),
            None,
            7,
            124,
            456,
            "successor prompt".into(),
            Some("provider-session-kept".into()),
            Some("tmux-kept".into()),
            None,
            None,
            0,
        );
        successor.turn_nonce = Some(format!("{nonce}-successor"));
        inflight::save_inflight_state(&successor).unwrap();
        assert_eq!(
            OperatorRelease::clear_late_projection(&PROVIDER, channel, &nonce),
            inflight::GuardedClearOutcome::Missing
        );
        assert_eq!(
            inflight::load_inflight_state_read_only(&PROVIDER, channel.get())
                .unwrap()
                .turn_nonce,
            successor.turn_nonce,
            "a successor row written in the same window must survive"
        );
    })
    .await;
}
