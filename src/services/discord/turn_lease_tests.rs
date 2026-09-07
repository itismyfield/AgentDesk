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
        let row = inflight::load_inflight_state(&PROVIDER, channel.get()).unwrap();
        inflight::clear_inflight_state_for_captured_episode(
            &PROVIDER,
            channel.get(),
            &inflight::InflightTurnIdentity::from_state(&row),
            token.turn_nonce(),
        );
        let error = release_on(&shared, &PROVIDER, channel, request)
            .await
            .unwrap_err();
        assert!(error.contains("matching inflight identity is missing"));
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
