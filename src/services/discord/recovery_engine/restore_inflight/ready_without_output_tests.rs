use super::*;
use crate::services::discord::{mailbox_finish_turn, mailbox_snapshot};

struct Fixture {
    state: inflight::InflightTurnState,
    shared: Arc<SharedData>,
    _env: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new(channel: u64) -> Self {
        let root = tempfile::tempdir().expect("runtime root");
        let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let mut state = super::tests::recovery_state(ProviderKind::Claude, channel);
        state.born_generation = 0;
        state.full_response = "published prefix\nunposted suffix".to_string();
        state.response_sent_offset = "published prefix\n".len();
        let output = root.path().join("source.jsonl");
        std::fs::write(&output, b"{\"type\":\"assistant\"}\n").expect("source");
        state.output_path = Some(output.to_string_lossy().into_owned());
        state.last_offset = std::fs::metadata(&output).expect("source EOF").len();
        state.turn_start_offset = Some(0);
        Self {
            state,
            shared,
            _env: env,
            _root: root,
        }
    }

    fn persist(&self) {
        inflight::save_inflight_state(&self.state).expect("persist recovery obligation");
        assert!(
            !output_has_bytes_after_offset(
                self.state.output_path.as_deref().expect("source"),
                self.state.last_offset,
            ),
            "fixture is source EOF, not unread JSONL"
        );
    }

    async fn claim(&self) {
        self.persist();
        assert!(
            super::super::reregister_active_turn_from_inflight(&self.shared, &self.state).await
        );
    }

    async fn settle<F, Fut>(&self, state: &inflight::InflightTurnState, relay: F) -> bool
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = RecoveryRelayOutcome>,
    {
        let owner = mailbox_snapshot(&self.shared, ChannelId::new(state.channel_id)).await;
        let mut captured = state.clone();
        captured.save_generation = self.load().expect("persisted fixture").save_generation;
        settle_ready_without_output_for_actor(
            &self.shared,
            &ProviderKind::Claude,
            &captured,
            owner.cancel_token.as_ref(),
            relay,
        )
        .await
    }

    fn load(&self) -> Option<inflight::InflightTurnState> {
        inflight::load_inflight_state(&ProviderKind::Claude, self.state.channel_id)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn partial_eof_delivers_only_unposted_response_and_releases_for_next_input() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for watcher_offset in [None, Some(19)] {
        let fixture = Fixture::new(5_071_801);
        let mut state = fixture.state.clone();
        state.last_watcher_relayed_offset = watcher_offset;
        fixture.claim().await;
        let expected = super::super::super::formatting::format_for_discord_with_provider(
            "unposted suffix",
            &ProviderKind::Claude,
        );
        let mut delivered = Vec::new();
        assert!(
            fixture
                .settle(&state, |text| {
                    assert!(
                        fixture.load().is_some(),
                        "obligation survives until transport completes"
                    );
                    delivered.push(text);
                    std::future::ready(RecoveryRelayOutcome::Delivered)
                },)
                .await
        );
        assert_eq!(
            delivered,
            vec![expected],
            "a positive prefix offset is not a terminal receipt"
        );
        assert!(fixture.load().is_none());
        let channel = ChannelId::new(state.channel_id);
        assert!(
            mailbox_snapshot(&fixture.shared, channel)
                .await
                .cancel_token
                .is_none()
        );
        let mut next = state;
        next.user_msg_id += 10;
        next.turn_nonce = Some("next-input".to_string());
        assert!(super::super::reregister_active_turn_from_inflight(&fixture.shared, &next).await);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn watcher_offset_at_eof_preserves_unsent_body_on_failed_delivery_then_retries() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut fixture = Fixture::new(5_071_802);
    fixture.state.response_sent_offset = 0;
    fixture.state.last_watcher_relayed_offset = Some(fixture.state.last_offset);
    fixture.claim().await;
    let expected = super::super::super::formatting::format_for_discord_with_provider(
        &fixture.state.full_response,
        &ProviderKind::Claude,
    );
    let mut attempts = Vec::new();
    for outcome in [
        RecoveryRelayOutcome::TransientFailure,
        RecoveryRelayOutcome::Delivered,
    ] {
        let state = fixture.load().expect("retryable obligation");
        assert!(
            fixture
                .settle(&state, |text| {
                    attempts.push(text);
                    std::future::ready(outcome)
                },)
                .await
        );
        if matches!(outcome, RecoveryRelayOutcome::TransientFailure) {
            let retained = fixture.load().expect("transient transport preserves row");
            assert_eq!(retained.full_response, fixture.state.full_response);
            assert!(!retained.terminal_delivery_completed());
            assert!(
                mailbox_snapshot(&fixture.shared, ChannelId::new(state.channel_id))
                    .await
                    .cancel_token
                    .is_some()
            );
        }
    }
    assert_eq!(attempts, vec![expected.clone(), expected]);
    assert!(fixture.load().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn committed_eof_skips_transport_but_unknown_or_restart_rows_remain_owned() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for case in 0..6 {
        let mut fixture = Fixture::new(5_072_803 + case);
        // Empty recovery starts empty; a published prefix cannot be rewound
        // to zero under the same durable turn identity.
        if case == 3 {
            fixture.state.full_response.clear();
            fixture.state.response_sent_offset = 0;
        }
        fixture.claim().await;
        match case {
            0 => fixture.state.terminal_delivery_committed = true,
            1 => fixture.state.response_sent_offset = fixture.state.full_response.len(),
            2 => fixture.state.response_sent_offset = usize::MAX,
            3 => {}
            4 => fixture
                .state
                .set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart),
            _ => fixture.state.rebind_origin = true,
        }
        // The canonical writer rejects invalid offsets; keep the valid durable
        // row while testing an invalid local recovery snapshot in case 2.
        if case != 2 {
            fixture.persist();
        }
        assert_eq!(
            fixture
                .settle(&fixture.state, |_| async {
                    panic!("committed, ambiguous, or separately owned rows must not POST")
                },)
                .await,
            case == 0
        );
        assert_eq!(fixture.load().is_none(), case == 0);
        assert_eq!(
            mailbox_snapshot(&fixture.shared, ChannelId::new(fixture.state.channel_id))
                .await
                .cancel_token
                .is_none(),
            case == 0
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn delivered_partial_eof_cannot_finish_or_clear_successor_during_transport() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let fixture = Fixture::new(5_071_804);
    fixture.claim().await;
    let channel = ChannelId::new(fixture.state.channel_id);
    let mut successor = fixture.state.clone();
    successor.user_msg_id += 10;
    successor.current_msg_id += 10;
    successor.turn_nonce = Some("successor-after-cancel".to_string());
    assert!(
        fixture
            .settle(&fixture.state, |_| async {
                mailbox_finish_turn(&fixture.shared, &ProviderKind::Claude, channel).await;
                inflight::save_inflight_state(&successor).expect("successor row");
                assert!(
                    super::super::reregister_active_turn_from_inflight(&fixture.shared, &successor)
                        .await
                );
                RecoveryRelayOutcome::Delivered
            },)
            .await
    );
    let remaining = fixture
        .load()
        .expect("successor row survives stale delivery callback");
    assert_eq!(remaining.turn_nonce, successor.turn_nonce);
    assert_eq!(
        mailbox_snapshot(&fixture.shared, channel)
            .await
            .active_user_message_id,
        Some(MessageId::new(successor.user_msg_id))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn partial_eof_actual_controller_preserves_frozen_prefix_and_streamed_current_anchor() {
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::recovery_paths::controller_cutover::{
        deliver_recovery_replace_via_controller, tests::RecoveryFakeGateway,
    };
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut fixture = Fixture::new(5_071_805);
    fixture.state.streaming_rollover_frozen_msg_ids = vec![40];
    fixture.state.current_msg_id = 41;
    // Rollover froze prefix in 40; 41 already shows the beginning of its suffix.
    // A normal streaming edit does not advance response_sent_offset.
    let mut messages = std::collections::BTreeMap::from([
        (MessageId::new(40), "published prefix".to_string()),
        (MessageId::new(41), "unposted".to_string()),
    ]);
    fixture.claim().await;
    let gateway = RecoveryFakeGateway::new(ReplaceLongMessageOutcome::EditedOriginal, true);
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let channel = ChannelId::new(fixture.state.channel_id);
    let context = RecoveryDeliveryContext::from_state(
        &fixture.shared,
        &ProviderKind::Claude,
        &fixture.state,
        None,
        fixture.shared.restart.current_generation,
    );
    assert!(
        fixture
            .settle(&fixture.state, |text| {
                let gateway = &gateway;
                let shared = &fixture.shared;
                let http = &http;
                let context = context.as_ref();
                async move {
                    deliver_recovery_replace_via_controller(
                        gateway,
                        shared,
                        &ProviderKind::Claude,
                        http,
                        channel,
                        MessageId::new(41),
                        &text,
                        context,
                    )
                    .await
                }
            },)
            .await
    );
    let replacements = gateway
        .replacements
        .lock()
        .expect("actual controller transport calls");
    assert_eq!(replacements.len(), 1);
    for (message, body) in replacements.iter() {
        messages.insert(*message, body.clone());
    }
    assert_eq!(
        messages.get(&MessageId::new(40)).unwrap(),
        "published prefix"
    );
    assert_eq!(
        messages.get(&MessageId::new(41)).unwrap(),
        &super::super::super::formatting::format_for_discord_with_provider(
            "unposted suffix",
            &ProviderKind::Claude,
        )
    );
    assert!(fixture.load().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn captured_live_partial_eof_preserves_failures_then_commits_and_clears_current_generation() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(5_071_900));
    let mut fixture = Fixture::new(5_071_806);
    fixture.state.born_generation = 5_071_900;
    fixture.claim().await;
    let channel = ChannelId::new(fixture.state.channel_id);
    let actor = mailbox_snapshot(&fixture.shared, channel)
        .await
        .cancel_token
        .expect("captured actor");
    for outcome in [
        RecoveryRelayOutcome::TransientFailure,
        RecoveryRelayOutcome::PermanentFailure,
        RecoveryRelayOutcome::Delivered,
    ] {
        let state = fixture.load().expect("live obligation");
        assert!(
            settle_ready_without_output_for_actor(
                &fixture.shared,
                &ProviderKind::Claude,
                &state,
                Some(&actor),
                |_| std::future::ready(outcome),
            )
            .await
        );
        if !matches!(outcome, RecoveryRelayOutcome::Delivered) {
            let retained = fixture
                .load()
                .expect("failed transport must preserve current row");
            assert!(!retained.terminal_delivery_completed());
            assert_eq!(retained.full_response, state.full_response);
            assert_eq!(
                retained.recovery_relay_attempts,
                state.recovery_relay_attempts + 1
            );
        }
    }
    assert!(
        fixture.load().is_none(),
        "captured live completion must not use reconcile's current-generation veto"
    );
    assert!(
        mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .is_none()
    );
    crate::services::discord::runtime_store::set_process_generation_for_tests(None);
}

#[tokio::test(flavor = "current_thread")]
async fn captured_partial_eof_never_commits_new_same_turn_progress_or_replacement_actor() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for replace_actor in [false, true] {
        let fixture = Fixture::new(5_071_807);
        fixture.claim().await;
        let channel = ChannelId::new(fixture.state.channel_id);
        let state = fixture.load().expect("captured row");
        let actor = mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .expect("captured actor");
        assert!(
            settle_ready_without_output_for_actor(
                &fixture.shared,
                &ProviderKind::Claude,
                &state,
                Some(&actor),
                |_| async {
                    let mut updated = state.clone();
                    updated.full_response.push_str(" newly arrived output");
                    if replace_actor {
                        mailbox_finish_turn(&fixture.shared, &ProviderKind::Claude, channel).await;
                        updated.user_msg_id += 10;
                        updated.turn_nonce = Some("replacement-actor".to_string());
                    }
                    inflight::save_inflight_state(&updated).expect("concurrent durable update");
                    if replace_actor {
                        assert!(
                            super::super::reregister_active_turn_from_inflight(
                                &fixture.shared,
                                &updated
                            )
                            .await
                        );
                    }
                    RecoveryRelayOutcome::Delivered
                },
            )
            .await
        );
        let remaining = fixture
            .load()
            .expect("progress after capture remains an obligation");
        assert!(remaining.full_response.ends_with(" newly arrived output"));
        assert!(!remaining.terminal_delivery_completed());
        assert!(
            mailbox_snapshot(&fixture.shared, channel)
                .await
                .cancel_token
                .is_some()
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn captured_partial_eof_all_outcomes_preserve_legacy_and_nonce_only_successors() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for (legacy, replace_actor) in [(true, true), (true, false), (false, true), (false, false)] {
        for outcome in [
            RecoveryRelayOutcome::Delivered,
            RecoveryRelayOutcome::PermanentFailure,
            RecoveryRelayOutcome::TransientFailure,
        ] {
            let mut fixture = Fixture::new(5_071_811);
            fixture.state.turn_nonce = (!legacy).then(|| "captured-A".to_string());
            fixture.claim().await;
            let channel = ChannelId::new(fixture.state.channel_id);
            let state = fixture.load().expect("captured A");
            let mut successor = state.clone();
            if legacy {
                successor.turn_start_offset = Some(1);
            } else {
                successor.turn_nonce = Some("successor-B".to_string());
            }
            let mut expected_durable = None;
            assert!(
                fixture
                    .settle(&state, |_| async {
                        if replace_actor {
                            mailbox_finish_turn(&fixture.shared, &ProviderKind::Claude, channel)
                                .await;
                        }
                        inflight::save_inflight_state(&successor).expect("successor");
                        if replace_actor {
                            assert!(
                                super::super::reregister_active_turn_from_inflight(
                                    &fixture.shared,
                                    &successor
                                )
                                .await
                            );
                        }
                        expected_durable = Some(
                            serde_json::to_value(fixture.load().expect("persisted B"))
                                .expect("B snapshot"),
                        );
                        outcome
                    })
                    .await
            );
            let surviving = fixture.load().expect("successor survives every outcome");
            assert_eq!(
                Some(serde_json::to_value(&surviving).expect("remaining snapshot")),
                expected_durable,
                "stale A cannot change even B retry budget or save generation"
            );
            assert_eq!(surviving.turn_nonce, successor.turn_nonce);
            assert_eq!(surviving.turn_start_offset, successor.turn_start_offset);
            assert_eq!(
                surviving.recovery_relay_attempts,
                successor.recovery_relay_attempts
            );
            assert!(!surviving.terminal_delivery_completed());
            assert!(
                mailbox_snapshot(&fixture.shared, channel)
                    .await
                    .cancel_token
                    .is_some()
            );
            mailbox_finish_turn(&fixture.shared, &ProviderKind::Claude, channel).await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn restart_partial_eof_preserves_unproven_actor_and_retries_ownerless_failures() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut fixture = Fixture::new(5_071_812);
    fixture.state.turn_nonce = None;
    fixture.claim().await;
    let state = fixture.load().expect("legacy row");
    assert!(
        !settle_ready_without_output(
            &fixture.shared,
            &ProviderKind::Claude,
            &state,
            |_| -> std::future::Ready<RecoveryRelayOutcome> {
                panic!("existing same-ID/NoneNonce actor cannot be adopted without an Arc witness")
            }
        )
        .await
    );
    let channel = ChannelId::new(state.channel_id);
    mailbox_finish_turn(&fixture.shared, &ProviderKind::Claude, channel).await;
    for outcome in [
        RecoveryRelayOutcome::PermanentFailure,
        RecoveryRelayOutcome::TransientFailure,
        RecoveryRelayOutcome::Delivered,
    ] {
        let before = fixture.load().expect("ownerless obligation");
        assert!(
            settle_ready_without_output(
                &fixture.shared,
                &ProviderKind::Claude,
                &before,
                |_| async { outcome }
            )
            .await
        );
        if !matches!(outcome, RecoveryRelayOutcome::Delivered) {
            let after = fixture.load().expect("failure preserves body");
            assert_eq!(after.full_response, before.full_response);
            assert_eq!(
                after.recovery_relay_attempts,
                before.recovery_relay_attempts + 1
            );
        }
    }
    assert!(fixture.load().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn captured_finalizer_refuses_same_id_legacy_recovery_actor_replacement() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for already_finalized in [false, true] {
        let mut fixture = Fixture::new(5_071_813 + u64::from(already_finalized));
        fixture.state.turn_nonce = None;
        fixture.claim().await;
        let channel = ChannelId::new(fixture.state.channel_id);
        let state = fixture.load().expect("A row");
        let original = mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .expect("original actor");
        let mut snapshot =
            super::super::super::turn_finalizer::SyntheticClaimSnapshot::from_row(&state);
        snapshot.recovery_actor = Some(Arc::downgrade(&original));
        if already_finalized {
            let _ = finish_recovered_turn_mailbox_for_captured_state(
                &fixture.shared,
                &ProviderKind::Claude,
                &state,
                snapshot.clone(),
            )
            .await;
        }
        let successor = Arc::new(CancelToken::from_persisted_turn_nonce(None));
        // RecoveryKickoff can replace the token without advancing turn_started_instant.
        // Only the actual actor comparison can protect this same-ID legacy successor.
        fixture
            .shared
            .mailbox(channel)
            .recovery_kickoff(
                successor.clone(),
                UserId::new(state.request_owner_user_id),
                Some(MessageId::new(state.effective_finalizer_turn_id())),
            )
            .await;
        let _ = finish_recovered_turn_mailbox_for_captured_state(
            &fixture.shared,
            &ProviderKind::Claude,
            &state,
            snapshot,
        )
        .await;
        let surviving = mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .expect("replacement survives finalizer");
        assert!(Arc::ptr_eq(&surviving, &successor));
        assert!(fixture.load().is_some());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn committed_partial_cas_cannot_clear_successor_inserted_during_finalizer_await() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut fixture = Fixture::new(5_071_815);
    fixture.state.turn_nonce = None;
    fixture.claim().await;
    let channel = ChannelId::new(fixture.state.channel_id);
    let original = mailbox_snapshot(&fixture.shared, channel)
        .await
        .cancel_token
        .expect("A actor");
    let mut committed = fixture.load().expect("A row");
    committed.terminal_delivery_committed = true;
    committed.response_sent_offset = committed.full_response.len();
    assert_eq!(
        inflight::save_inflight_state_if_identity_unchanged(
            &mut committed,
            "partial test confirmed transport"
        ),
        inflight::GuardedSaveOutcome::Saved
    );
    let mut snapshot =
        super::super::super::turn_finalizer::SyntheticClaimSnapshot::from_row(&committed);
    snapshot.recovery_actor = Some(Arc::downgrade(&original));
    let mut successor = committed.clone();
    successor.current_msg_id += 1;
    successor.full_response.push_str(" successor body");
    successor.terminal_delivery_committed = false;
    let mut expected = None;
    retire_captured_ready_response(
        &fixture.shared,
        &ProviderKind::Claude,
        &committed,
        snapshot,
        |snapshot| async {
            let outcome = finish_recovered_turn_mailbox_for_captured_state(
                &fixture.shared,
                &ProviderKind::Claude,
                &committed,
                snapshot,
            )
            .await;
            assert!(matches!(
                outcome,
                Some(
                    super::super::super::turn_finalizer::FinalizeOutcome::Finalized {
                        removed_token: Some(_),
                        ..
                    }
                )
            ));
            inflight::save_inflight_state(&successor).expect("B row after CAS and finalizer");
            assert!(
                super::super::reregister_active_turn_from_inflight(&fixture.shared, &successor)
                    .await
            );
            expected =
                Some(serde_json::to_value(fixture.load().expect("B row")).expect("B snapshot"));
            outcome
        },
    )
    .await;
    assert_eq!(
        Some(
            serde_json::to_value(fixture.load().expect("B survives stale row retirement"))
                .expect("remaining snapshot")
        ),
        expected
    );
    let actor = mailbox_snapshot(&fixture.shared, channel)
        .await
        .cancel_token
        .expect("B actor");
    assert!(!Arc::ptr_eq(&actor, &original));
}

#[tokio::test(flavor = "current_thread")]
async fn partial_eof_actual_fallback_uses_own_anchor_snapshot_and_refuses_foreign_writes() {
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::recovery_paths::controller_cutover::{
        deliver_recovery_replace_via_controller, tests::RecoveryFakeGateway,
    };
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for successor_stage in 0..4 {
        let mut fixture = Fixture::new(5_072_820 + successor_stage);
        fixture.state.turn_nonce = None;
        fixture.claim().await;
        let state = fixture.load().expect("captured A");
        let channel = ChannelId::new(state.channel_id);
        let actor = mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .expect("A actor");
        let context = RecoveryDeliveryContext::from_state(
            &fixture.shared,
            &ProviderKind::Claude,
            &state,
            None,
            fixture.shared.restart.current_generation,
        )
        .expect("context")
        .capture_anchor_updates(&state);
        let gateway = RecoveryFakeGateway::new(
            ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "404 stale anchor".to_string(),
                replacement_anchor: Some(MessageId::new(5_072_920 + successor_stage)),
            },
            true,
        );
        let replacement_actor = Arc::new(CancelToken::from_persisted_turn_nonce(None));
        let gateway = if successor_stage == 3 {
            let shared = fixture.shared.clone();
            let replacement = replacement_actor.clone();
            let user = MessageId::new(state.effective_finalizer_turn_id());
            let owner = UserId::new(state.request_owner_user_id);
            gateway.before_replace_returns(move || {
                Box::pin(async move {
                    // Only the in-memory actor changes during the transport await.
                    // The durable row, including save generation, remains untouched.
                    shared
                        .mailbox(channel)
                        .recovery_kickoff(replacement, owner, Some(user))
                        .await;
                })
            })
        } else {
            gateway
        };
        let row_path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().expect("runtime root"),
            &ProviderKind::Claude,
            state.channel_id,
        );
        let original_bytes = std::fs::read(&row_path).expect("original durable bytes");
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let mut expected_successor =
            (successor_stage == 3).then(|| serde_json::to_value(&state).expect("unchanged row"));
        assert!(
            settle_ready_without_output_for_actor(
                &fixture.shared,
                &ProviderKind::Claude,
                &state,
                Some(&actor),
                |text| {
                    let fixture = &fixture;
                    let gateway = &gateway;
                    let http = &http;
                    let state = &state;
                    let context = &context;
                    let expected_successor = &mut expected_successor;
                    async move {
                        if successor_stage == 1 {
                            let mut successor = fixture.load().expect("before transport");
                            successor.full_response.push_str(" B before bind");
                            inflight::save_inflight_state(&successor)
                                .expect("foreign write before own anchor bind");
                            *expected_successor = Some(
                                serde_json::to_value(fixture.load().expect("B"))
                                    .expect("B snapshot"),
                            );
                        }
                        let outcome = deliver_recovery_replace_via_controller(
                            gateway,
                            &fixture.shared,
                            &ProviderKind::Claude,
                            http,
                            channel,
                            MessageId::new(state.current_msg_id),
                            &text,
                            Some(context),
                        )
                        .await;
                        assert!(matches!(outcome, RecoveryRelayOutcome::Delivered));
                        let pending_anchor = context.pending_anchor_after_delivery();
                        assert!(
                            pending_anchor.is_some(),
                            "confirmed fallback waits for actor-validated binding"
                        );
                        let current = fixture.load().expect("row untouched by transport callback");
                        assert_eq!(current.current_msg_id, state.current_msg_id);
                        if successor_stage != 1 {
                            assert_eq!(current.save_generation, state.save_generation);
                        }
                        if successor_stage == 2 {
                            let mut successor = fixture.load().expect("after confirmed fallback");
                            successor.full_response.push_str(" B after receipt");
                            inflight::save_inflight_state(&successor)
                                .expect("foreign write before actor-validated bind");
                            *expected_successor = Some(
                                serde_json::to_value(fixture.load().expect("B"))
                                    .expect("B snapshot"),
                            );
                        }
                        CapturedRecoveryDelivery {
                            outcome,
                            pending_anchor,
                        }
                    }
                }
            )
            .await
        );
        assert_eq!(
            gateway.replacements.lock().expect("transport calls").len(),
            1
        );
        if successor_stage == 0 {
            assert!(
                fixture.load().is_none(),
                "own fallback bind must not strand delivered A"
            );
            assert!(
                mailbox_snapshot(&fixture.shared, channel)
                    .await
                    .cancel_token
                    .is_none()
            );
        } else {
            assert_eq!(
                Some(serde_json::to_value(fixture.load().expect("B survives")).expect("remaining")),
                expected_successor
            );
            let surviving = mailbox_snapshot(&fixture.shared, channel)
                .await
                .cancel_token
                .expect("active actor remains");
            if successor_stage == 3 {
                assert!(Arc::ptr_eq(&surviving, &replacement_actor));
                assert_eq!(
                    std::fs::read(&row_path).expect("surviving bytes"),
                    original_bytes,
                    "actor-only handoff must not bind the old fallback anchor into B's adopted row"
                );
            } else {
                assert!(Arc::ptr_eq(&surviving, &actor));
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn captured_episode_claim_preserves_actor_witness_and_refuses_mismatched_row_cleanup() {
    use crate::services::discord::turn_finalizer::{TurnKey, claim_normal_episode};
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for replace in [false, true] {
        let mut fixture = Fixture::new(5_072_850 + replace as u64);
        fixture.state.turn_nonce = None;
        fixture.claim().await;
        let state = fixture.load().expect("A row");
        let channel = ChannelId::new(state.channel_id);
        let original = mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .expect("A actor");
        let replacement = Arc::new(CancelToken::from_persisted_turn_nonce(None));
        if replace {
            fixture
                .shared
                .mailbox(channel)
                .recovery_kickoff(
                    replacement.clone(),
                    UserId::new(state.request_owner_user_id),
                    Some(MessageId::new(state.effective_finalizer_turn_id())),
                )
                .await;
        }
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().expect("root"),
            &ProviderKind::Claude,
            state.channel_id,
        );
        let before = std::fs::read(&path).expect("captured row bytes");
        let result = claim_normal_episode(
            &fixture.shared,
            &ProviderKind::Claude,
            TurnKey::new(
                channel,
                state.effective_finalizer_turn_id(),
                fixture.shared.restart.current_generation,
            )
            .with_episode_nonce(None),
            true,
            Some(original.clone()),
        )
        .await;
        if replace {
            assert!(
                result.is_err(),
                "actor mismatch must refuse before clear_inflight"
            );
            assert_eq!(std::fs::read(&path).expect("B row survives"), before);
            let active = mailbox_snapshot(&fixture.shared, channel)
                .await
                .cancel_token
                .expect("B actor survives");
            assert!(Arc::ptr_eq(&active, &replacement));
        } else {
            let captured = result.ok().flatten().expect("original actor claimed");
            let witness = captured
                .snapshot
                .expect("captured row")
                .recovery_actor
                .expect("original actor witness")
                .upgrade()
                .expect("original still held");
            assert!(Arc::ptr_eq(&witness, &original));
            assert!(fixture.load().is_none());
        }
    }
}
