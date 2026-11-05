//! The token a routine start and an exact retry hand their writer carries the first-key gate.
use super::*;
use crate::services::claude_tui::input::submission::tests::gated_warm_write;

/// A stop that took the gate leaves no key; a writer that keyed first leaves MayHaveWritten.
fn assert_gate(case: &str, token: &Arc<CancelToken>, stop: bool, sends: &[String]) {
    let sink = token.native_stop.receipt_sink();
    if stop {
        assert!(sends.is_empty(), "{case}: no key {sends:?}");
        assert!(sink.stopped_before_key(), "{case}");
    } else {
        assert_eq!(sends, ["load", "paste", "Enter"], "{case}");
        assert!(sink.may_have_written(), "{case}: the writer keyed first");
    }
}

/// A routine start's claim hands its writer the armed token it registered.
#[tokio::test(flavor = "current_thread")]
async fn a_routine_starts_token_gates_its_writers_first_key() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    for (n, stop) in [true, false].into_iter().enumerate() {
        let channel = ChannelId::new(7_795_301 + n as u64);
        let reservation = crate::services::discord::router::reserve_headless_turn();
        let claim = crate::services::discord::router::claim_reserved_headless_turn;
        let claimed = claim(
            &shared,
            channel,
            UserId::new(7),
            &reservation,
            (ProviderKind::Claude, None),
            None,
        )
        .await;
        let Ok((_transition, token)) = claimed else {
            panic!("an idle channel claims");
        };
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
        let registered = snapshot
            .cancel_token
            .expect("the claim registered its token");
        assert!(Arc::ptr_eq(&token, &registered));
        let sends = gated_warm_write(&token, stop);
        assert_gate("routine", &token, stop, &sends);
    }
}

/// An exact retry's start registers an armed token, and its writer takes that token's gate.
#[test]
fn an_exact_retrys_token_gates_its_writers_first_key_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::",
        "first_key_gate_paths::an_exact_retrys_token_gates_its_writers_first_key_pg"
    )) {
        return;
    }
    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
        let channels = [7_795_401, 7_795_402].map(ChannelId::new);
        let host = host(&channels).await;
        for (channel, stop) in channels.into_iter().zip([true, false]) {
            let _active = test_activation::activate(channel.get());
            let message = channel.get() + 100;
            let key = format!("discord:claude:{channel}:{message}");
            let sources = vec![message.to_string(), (message + 1).to_string()];
            let (attempt, nonce) = started(&host, key, channel, sources, "exact gate").await;
            let session = session_key(&host, channel).await;
            let receipt_id = attempt.receipt_id();
            seed_session(
                &host,
                &session,
                &channel.to_string(),
                Some((receipt_id, &nonce)),
            )
            .await;
            let mut observation = RawObservation::for_attempt(&attempt);
            for seen in [
                Seen::StdoutEof,
                Seen::Exited { waited: true },
                Seen::StderrDrained,
            ] {
                observation.record(seen);
            }
            let evidence = observation.no_effect_evidence().unwrap();
            let ack = write::classify_no_effect(&host.pool, attempt, &evidence).await;
            let retry = ExactRetry {
                user_message_id: MessageId::new(message),
                sources: vec![MessageId::new(message + 1)],
                request_owner: UserId::new(7),
                text: "exact gate".into(),
                reply_context: None,
                permit: RetryPermit::from_no_effect(ack.unwrap()),
            };
            let gateway = crate::services::discord::gateway::DiscordGateway::new(
                host.api.http.clone(),
                host.shared.clone(),
                ProviderKind::Claude,
                None,
            );
            let handoff =
                crate::services::discord::gateway::TurnGateway::schedule_exact_retry_with_history(
                    &gateway, channel, retry,
                );
            let (handoff, started) = start(channel, handoff).await;
            assert_eq!(handoff, ExactHandoff::Started);
            let (_, release) = started.expect("the retry reached its provider");
            // The provider is held here, so the mailbox still holds the retry's own token.
            let snapshot = crate::services::discord::mailbox_snapshot(&host.shared, channel).await;
            let token = snapshot
                .cancel_token
                .expect("the retry registered its token");
            let sends = gated_warm_write(&token, stop);
            let _ = release.send(());
            assert_gate("exact retry", &token, stop, &sends);
        }
        host.db.drop().await;
    });
}
