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
            settle_ready_without_output(&fixture.shared, &ProviderKind::Claude, &state, |text| {
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
            settle_ready_without_output(&fixture.shared, &ProviderKind::Claude, &state, |text| {
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
        let mut fixture = Fixture::new(5_071_803);
        fixture.claim().await;
        match case {
            0 => fixture.state.terminal_delivery_committed = true,
            1 => fixture.state.response_sent_offset = fixture.state.full_response.len(),
            2 => fixture.state.response_sent_offset = usize::MAX,
            3 => fixture.state.full_response.clear(),
            4 => fixture
                .state
                .set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart),
            _ => fixture.state.rebind_origin = true,
        }
        fixture.persist();
        assert_eq!(
            settle_ready_without_output(
                &fixture.shared,
                &ProviderKind::Claude,
                &fixture.state,
                |_| async {
                    panic!("committed, ambiguous, or separately owned rows must not POST")
                },
            )
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
        settle_ready_without_output(
            &fixture.shared,
            &ProviderKind::Claude,
            &fixture.state,
            |_| async {
                mailbox_finish_turn(&fixture.shared, &ProviderKind::Claude, channel).await;
                inflight::save_inflight_state(&successor).expect("successor row");
                assert!(
                    super::super::reregister_active_turn_from_inflight(&fixture.shared, &successor)
                        .await
                );
                RecoveryRelayOutcome::Delivered
            },
        )
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
