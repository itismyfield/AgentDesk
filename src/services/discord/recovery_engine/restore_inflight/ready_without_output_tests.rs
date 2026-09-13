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
        !settle_ready_without_output(&fixture.shared, &ProviderKind::Claude, &state, |_| async {
            panic!("existing same-ID/NoneNonce actor cannot be adopted without an Arc witness")
        })
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
    let mut fixture = Fixture::new(5_071_813);
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
    finish_recovered_turn_mailbox_for_captured_state(
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
