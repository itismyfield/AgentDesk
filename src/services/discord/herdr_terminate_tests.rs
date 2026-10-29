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
                .await
                .map(drop),
                Ok(())
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
        assert_eq!(settle_and_release(&shared, &ProviderKind::Claude, "execution-a", captured, transition).await.map(drop), Ok(()));
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
                .await
                .map(drop),
                Ok(())
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

#[test]
fn a2_t11_terminate_settlement_accepts_absence_and_preserves_an_accepted_body() {
    use crate::services::discord::gateway::{DiscordGateway, TurnGateway};
    use crate::services::discord::{
        inflight as disk,
        recovery_engine::{
            BodyCreateTestRemote, a2_admitted_for_tmux, a2_post_captured_without_bind,
            a2_stale_captured_recovery_is_held,
        },
    };
    let name = concat!(
        module_path!(),
        "::a2_t11_terminate_settlement_accepts_absence_and_preserves_an_accepted_body"
    );
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let source_root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for stage in 0..3 {
            let mut shared=super::super::make_shared_data_for_tests();
            Arc::get_mut(&mut shared).unwrap().api_port=0;
            let channel=ChannelId::new(75_072_140+stage);
            let remote=BodyCreateTestRemote::new(channel.get()).await;
            let _http=super::super::shared_state::test_rest::install(remote.http.clone());
            let gateway=DiscordGateway::new(remote.http.clone(),shared.clone(),ProviderKind::Claude,None);
            let current=if stage==2 {gateway.send_message(channel,"owned partial body").await.unwrap().get()} else {0};
            let row=if stage==1 {
                a2_admitted_for_tmux(source_root.path(),channel.get(),"accepted terminal body before bind",0,&format!("a2-term-{stage}")).await
            } else {
                let mut row=disk::InflightTurnState::new(ProviderKind::Claude,channel.get(),None,7,channel.get()+1,current,"original prompt".into(),None,None,None,None,0);
                row.turn_nonce=Some(format!("a2-term-{stage}"));disk::save_inflight_state(&row).unwrap();row
            };
            let token=Arc::new(CancelToken::from_persisted_turn_nonce(row.turn_nonce.clone()));
            shared.mailbox(channel).restore_active_turn(token.clone(),UserId::new(7),MessageId::new(row.user_msg_id)).await;
            shared.restart.global_active.store(1,Ordering::Relaxed);
            let key=TurnKey::new(channel,row.user_msg_id,shared.restart.current_generation).with_episode_nonce(row.turn_nonce.as_deref());
            shared.turn_finalizer.register_start_with_completion_admission(key,ProviderKind::Claude,disk::RelayOwnerKind::None,super::super::turn_finalizer::CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,&shared);
            let actual=if stage==1 {Some(a2_post_captured_without_bind(&remote,&shared,&row).await)} else {disk::opt_message_id(current)};
            let captured=capture_turn(&shared,&ProviderKind::Claude,channel).await.unwrap().expect("actual terminate captures the active episode even with response0");
            let visible=remote.messages.lock().unwrap().clone();
            let calls=remote.calls.lock().unwrap().len();
            let execution=format!("a2-terminated-execution-{stage}");
            crate::services::claude::herdr_turn::hold(&execution).unwrap();
            let transition=shared.session_transition_lock(channel).lock_owned().await;
            assert_eq!(settle_and_release(&shared,&ProviderKind::Claude,&execution,Some(captured),transition).await.map(drop),Ok(()));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed),0);
            assert!(shared.mailbox(channel).snapshot().await.cancel_token.is_none());
            assert!(disk::load_inflight_state_read_only(&ProviderKind::Claude,channel.get()).is_none());
            assert_eq!(*remote.messages.lock().unwrap(),visible);
            assert_eq!(remote.calls.lock().unwrap().len(),calls,"termination settlement never transports a body or a fake anchor");
            if let Some(actual)=actual {assert!(remote.messages.lock().unwrap().iter().any(|message|message["id"]==actual.get().to_string()));}
            if stage==1 {
                assert!(a2_stale_captured_recovery_is_held(&remote,&shared,&row).await);
                assert_eq!(remote.calls.lock().unwrap().len(),calls);
                assert!(disk::load_inflight_state_read_only(&ProviderKind::Claude,channel.get()).is_none());
            }
        }
    });
}

#[test]
fn a2_t11_late_terminate_never_releases_an_absent_successor() {
    let name = concat!(
        module_path!(),
        "::a2_t11_late_terminate_never_releases_an_absent_successor"
    );
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut shared = super::super::make_shared_data_for_tests();
            Arc::get_mut(&mut shared).unwrap().api_port = 0;
            let channel = ChannelId::new(75_072_145);
            let remote =
                super::super::recovery_engine::BodyCreateTestRemote::new(channel.get()).await;
            let _http = super::super::shared_state::test_rest::install(remote.http.clone());
            let a = test_turn::start(&shared, channel, 51, "a2-old").await;
            let mut original = super::super::inflight::load_inflight_state_read_only(
                &ProviderKind::Claude,
                channel.get(),
            )
            .unwrap();
            original.current_msg_id = 0;
            let path = super::super::inflight::inflight_state_path(
                &super::super::inflight::inflight_runtime_root().unwrap(),
                &ProviderKind::Claude,
                channel.get(),
            );
            std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
            let captured = capture_turn(&shared, &ProviderKind::Claude, channel)
                .await
                .unwrap()
                .expect("real terminate captures current0");
            let successor = Arc::new(std::sync::Mutex::new(None));
            let (s, slot, path2) = (shared.clone(), successor.clone(), path.clone());
            probe_settlement_window(Box::new(move || {
                Box::pin(async move {
                    test_turn::finish_naturally(&s, channel, 51, "a2-old", &a).await;
                    let b = test_turn::start(&s, channel, 61, "a2-new").await;
                    let mut row = super::super::inflight::load_inflight_state_read_only(
                        &ProviderKind::Claude,
                        channel.get(),
                    )
                    .unwrap();
                    row.current_msg_id = 0;
                    std::fs::write(&path2, serde_json::to_vec(&row).unwrap()).unwrap();
                    *slot.lock().unwrap() = Some((b, std::fs::read(&path2).unwrap()));
                })
            }));
            crate::services::claude::herdr_turn::hold("a2-old-execution").unwrap();
            let transition = shared.session_transition_lock(channel).lock_owned().await;
            assert_eq!(
                settle_and_release(
                    &shared,
                    &ProviderKind::Claude,
                    "a2-old-execution",
                    Some(captured),
                    transition
                )
                .await
                .map(drop),
                Ok(())
            );
            let (b, bytes) = successor.lock().unwrap().take().unwrap();
            assert!(Arc::ptr_eq(
                &test_turn::active(&shared, channel).await.unwrap(),
                &b
            ));
            assert!(!b.cancelled.load(Ordering::Relaxed));
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
            assert!(!remote.calls.lock().unwrap().iter().any(
                |(route, _)| route.starts_with("POST")
                    || route.starts_with("PATCH")
                    || (route.starts_with("DELETE") && !route.contains("/reactions/"))
            ));
        });
}
