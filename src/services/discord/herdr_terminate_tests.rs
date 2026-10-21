use super::*;
use serenity::model::id::{MessageId, UserId};
use std::sync::atomic::Ordering;

#[test]
fn m1_disabled_service_has_zero_io() {
    let _lock = crate::config::shared_test_env_lock();
    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let pool = sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://postgres@127.0.0.1:1/absent")
                .unwrap();
            let result = terminate_explicit_herdr(
                OperatorTerminate {
                    session_key: "missing".into(),
                    execution_nonce: "nonce".into(),
                },
                shared,
                &pool,
            )
            .await;
            assert_eq!(result, TerminationResult::Disabled);
            pool.close().await;
        });
}

#[tokio::test]
async fn m1_retire_cas_failure_preserves_successor_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = "5340115";
    let owner = hosted_execution::tests::owner(channel);
    let record = hosted_execution::tests::record(&owner, "execution-cas", HostedState::Bound);
    let key = "claude/discord_0123456789abcdef/test-node:AgentDesk-claude-m1cas";
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)")
        .bind(key).bind(&owner.discord_token_hash).bind(channel)
        .bind(serde_json::to_value(&record).unwrap()).execute(&pool).await.unwrap();
    let row = load(&pool, key).await.unwrap();
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
        .bind(key)
        .bind(
            serde_json::to_value(hosted_execution::tests::record(
                &owner,
                "successor-cas",
                HostedState::Bound,
            ))
            .unwrap(),
        )
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        retire_confirmed(&pool, &row, &record).await,
        Err(TerminationResult::Indeterminate(_))
    ));
    let live = load(&pool, key).await.unwrap();
    assert!(
        matches!(live.record, HostedRecord::Known(r) if r.state == HostedState::Bound && r.execution_nonce == "successor-cas")
    );
    pool.close().await;
    db.drop().await;
}

#[test]
fn m1_hold_release_window_preserves_input_until_settlement() {
    let _lock = crate::config::shared_test_env_lock();
    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let channel = ChannelId::new(5_340_114);
            let transition = shared.session_transition_lock(channel).lock_owned().await;
            crate::services::claude::herdr_turn::hold("execution-window").unwrap();
            test_queue::queue_original(&shared, channel).await;
            let opened = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (probe_shared, probe_opened) = (shared.clone(), opened.clone());
            probe_settlement_window(Box::new(move || {
                Box::pin(async move {
                    assert!(
                        crate::services::claude::herdr_turn::input_holds()
                            .unwrap()
                            .is_empty(),
                        "the window opens only after the hold is gone"
                    );
                    assert_eq!(
                        test_queue::take_queued(&probe_shared, channel).await,
                        None,
                        "input arriving before settlement must not start"
                    );
                    assert_eq!(
                        test_queue::queued_texts(&probe_shared, channel).await,
                        vec![test_queue::ORIGINAL.to_string()]
                    );
                    probe_opened.store(true, Ordering::Relaxed);
                })
            }));
            assert_eq!(
                settle_and_release(
                    &shared,
                    &ProviderKind::Claude,
                    "execution-window",
                    None,
                    transition
                )
                .await,
                TerminationResult::Retired
            );
            assert!(opened.load(Ordering::Relaxed));
            assert_eq!(
                test_queue::take_queued(&shared, channel).await.as_deref(),
                Some(test_queue::ORIGINAL)
            );
            assert_eq!(test_queue::take_queued(&shared, channel).await, None);
        });
}

#[test]
fn m1_host_terminated_settles_exact_turn_and_publishes_queue_once() {
    let _lock = crate::config::shared_test_env_lock();
    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let shared = super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(5_340_113);
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some("turn-a".into())));
        shared.mailbox(channel).restore_active_turn(token.clone(), UserId::new(7), MessageId::new(51)).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let key = TurnKey::new(channel, 51, shared.restart.current_generation).with_episode_nonce(Some("turn-a"));
        shared.turn_finalizer.register_start_with_completion_admission(key, ProviderKind::Claude,
            super::super::inflight::RelayOwnerKind::Watcher,
            super::super::turn_finalizer::CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
            &shared);
        let mut row = super::super::inflight::InflightTurnState::new(ProviderKind::Claude,
            channel.get(), None, 7, 51, 52, "original prompt".into(), None, None, None, None, 0);
        row.turn_nonce = Some("turn-a".into());
        super::super::inflight::save_inflight_state(&row).unwrap();
        let captured = capture_turn(&shared, &ProviderKind::Claude, channel).await.unwrap();
        crate::services::claude::herdr_turn::hold("execution-a").unwrap();
        let mut events = super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
        test_queue::queue_original(&shared, channel).await;
        let transition = shared.session_transition_lock(channel).lock_owned().await;
        let probe_shared = shared.clone();
        probe_settlement_window(Box::new(move || Box::pin(async move {
            assert!(probe_shared.session_transition_lock(channel).try_lock_owned().is_err(), "transition stays owned until settlement");
            assert_eq!(test_queue::take_queued(&probe_shared, channel).await, None);
        })));
        assert_eq!(settle_and_release(&shared, &ProviderKind::Claude, "execution-a", captured, transition).await, TerminationResult::Retired);
        assert!(crate::services::claude::herdr_turn::input_holds().unwrap().is_empty());
        let mut eligible = 0;
        while let Ok(event) = events.try_recv() { if event.queue_is_eligible() { eligible += 1; } }
        assert_eq!(eligible, 1, "host settlement authorizes exactly one queue edge despite missing output barriers");
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(shared.mailbox(channel).snapshot().await.cancel_token.is_none());
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(test_queue::take_queued(&shared, channel).await.as_deref(), Some(test_queue::ORIGINAL), "the settled channel kicks its queue exactly once");
        assert_eq!(test_queue::take_queued(&shared, channel).await, None);
    });
}

// A natural terminal settled A and B took the channel before the late HostTerminated for A:
// that duplicate settles nothing of B and still reports the retire.
#[test]
fn m2_natural_a_then_b_late_settlement_preserves_b() {
    let _lock = crate::config::shared_test_env_lock();
    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let channel = ChannelId::new(5_340_116);
            let token_a = test_turn::start(&shared, channel, 51, "turn-a").await;
            let captured = capture_turn(&shared, &ProviderKind::Claude, channel)
                .await
                .unwrap();
            crate::services::claude::herdr_turn::hold("execution-a").unwrap();
            crate::services::claude::herdr_turn::hold("execution-b").unwrap();
            let transition = shared.session_transition_lock(channel).lock_owned().await;
            let token_b = Arc::new(std::sync::Mutex::new(None));
            let (probe_shared, probe_a, probe_b) =
                (shared.clone(), token_a.clone(), token_b.clone());
            probe_settlement_window(Box::new(move || {
                Box::pin(async move {
                    test_turn::finish_naturally(&probe_shared, channel, 51, "turn-a", &probe_a)
                        .await;
                    let b = test_turn::start(&probe_shared, channel, 61, "turn-b").await;
                    *probe_b.lock().unwrap() = Some(b);
                })
            }));
            assert_eq!(
                settle_and_release(
                    &shared,
                    &ProviderKind::Claude,
                    "execution-a",
                    captured,
                    transition
                )
                .await,
                TerminationResult::Retired
            );
            let token_b = token_b
                .lock()
                .unwrap()
                .take()
                .expect("B started in the window");
            let active = test_turn::active(&shared, channel).await.expect("B active");
            assert!(Arc::ptr_eq(&active, &token_b));
            assert!(!token_b.cancelled.load(Ordering::Relaxed));
            assert_eq!(token_b.turn_nonce(), Some("turn-b"));
            assert_eq!(
                test_turn::inflight(channel),
                Some((61, Some("turn-b".into())))
            );
            let holds = crate::services::claude::herdr_turn::input_holds().unwrap();
            assert!(holds.iter().any(|(nonce, _)| nonce == "execution-b"));
            assert!(!holds.iter().any(|(nonce, _)| nonce == "execution-a"));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        });
}

// A mailbox actor that never answers is not an idle channel: capture refuses instead of
// proceeding as if no turn were running.
#[test]
fn m2_unreadable_mailbox_is_never_an_idle_channel() {
    let _lock = crate::config::shared_test_env_lock();
    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let channel = ChannelId::new(5_340_117);
            shared.mailboxes.insert_unreachable_for_test(channel);
            assert!(matches!(
                capture_turn(&shared, &ProviderKind::Claude, channel).await,
                Err(why) if why == "mailbox unreadable"
            ));
        });
}
