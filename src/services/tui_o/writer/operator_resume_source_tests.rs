use super::*;

#[test]
fn local_retry_managed_verified_host_drains_budget_before_following_exact_work() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::local_retry_managed_verified_host_drains_budget_before_following_exact_work"
    )) {
        return;
    }
    use crate::services::tui_o::writer::host::{self, HostParts, Readiness, test_io::TestHost};
    use dedupe::codex_verified::{
        VerifiedSourceSnapshot,
        provenance::{CodexEpisodeSpan, TurnEpisodeRef, admission},
    };
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _fake_tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (context, canonical, retained) = durable_canary_without_launch_files(root.path());
    std::fs::hard_link(&retained, &canonical).unwrap();
    crate::services::discord::runtime_store::fsync_parent_dir(&canonical).unwrap();
    publish_nonce(&context);
    dedupe::register_tmux_channel(CANARY_TMUX, CANARY_CHANNEL);
    dedupe::set_codex_delivery_permission_for_tests(
        &context,
        dedupe::CodexDeliveryPermissionForTests::Allowed,
    );
    dedupe::resolve_codex_claims();
    let proof = crate::services::tmux_common::with_tmux_source_authority(
        CANARY_TMUX,
        VerifiedSourceSnapshot::capture,
    )
    .unwrap();
    let source = proof.execution().source.clone();
    assert!(dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &source
    ));
    let _selected = crate::services::tui_o::cutover::test_override::force_channels(&[(
        CANARY_CHANNEL,
        crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
    )]);
    tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap().block_on(async {
        for retired in [None, Some(true), Some(false)] {
            let bytes = std::fs::read(&source.path).unwrap();
            let harness = Harness::build_channel(CANARY_CHANNEL, |_| vec![InitSource {
                source_id: source.clone(), delivery_start: bytes.len() as u64,
                prefix_hash: hex::encode(Sha256::digest(&bytes)),
            }]);
            let mut store = harness.channel();
            let mut key = unit("missing root");
            key.channel_id = CANARY_CHANNEL;
            key.provider = ShadowProvider::Codex;
            store.append_ledger(LedgerEntry::Prepared {
                notice_origin: None, serial: 0, unit_key: key, piece_index: 0,
                payload: "local saved payload".into(), anchor_id: 100, epoch: 1,
                local_retry: Some(crate::services::tui_o::store::ledger::LocalRetryMark::Budget) }).unwrap();
            store.append_ledger(LedgerEntry::NotFound { serial: 0 }).unwrap();
            store.init_codex_provenance().unwrap();
            if let Some(retired) = retired {
                let path = harness._runtime.path().join("disallowed.jsonl");
                std::fs::write(&path, b"").unwrap();
                let old = source_id_for("retired", &path).unwrap();
                assert!(!dedupe::codex_verified_o_source_allowed(CANARY_CHANNEL, &old));
                store.attach_source(&old).unwrap();
                store.set_retired(&old, retired).unwrap();
            }
            let start = std::fs::metadata(&source.path).unwrap().len();
            append(&source.path, &output_line("event_msg", serde_json::json!({"type":"task_started","turn_id":"following"})));
            append(&source.path, &output_line("response_item", serde_json::json!({"type":"message","role":"assistant","id":"exact","content":[{"type":"output_text","text":"following exact"}]})));
            append(&source.path, &output_line("event_msg", serde_json::json!({"type":"task_complete","turn_id":"following"})));
            let episode = TurnEpisodeRef { channel_id: CANARY_CHANNEL, user_message_id: 711,
                request_owner_id: 8, turn_nonce: "retry-following".into(), native_turn_id: "following".into() };
            let span = CodexEpisodeSpan { execution: proof.execution().clone(), episode: episode.clone(),
                delivery_channel_id: CANARY_CHANNEL, offset_authority_channel_id: CANARY_CHANNEL,
                generation_mtime_ns: 1, start, end: Some(std::fs::metadata(&source.path).unwrap().len()) };
            store.persist_codex_span(span).unwrap();
            drop(store);
            let io = TestHost::new([(CANARY_CHANNEL, source.clone())]);
            io.sessions.lock().unwrap().insert(CANARY_CHANNEL, CANARY_TMUX.into());
            harness.gate.acquired();
            let ready = Arc::new(Readiness::default());
            assert!(!admission::owed_delivery_ready(CANARY_CHANNEL), "exact delivery starts disabled");
            admission::set_ready_for_tests(CANARY_CHANNEL, true);
            let mut hosts = host::start_managed(ShadowProvider::Codex, true,
                crate::services::tui_o::cutover::boot_ownership(), || HostParts {
                    io: io.clone(), runtime_root: Some(harness._runtime.path().into()),
                    gate: harness.gate.clone(), readiness: ready.clone(),
                });
            assert_eq!(hosts.len(), 1);
            polls(3).await;
            assert!(ready.submission(CANARY_CHANNEL).is_some(), "real host installed commands receiver");
            let token = Arc::new(crate::services::provider::CancelToken::from_persisted_turn_nonce(Some(episode.turn_nonce.clone())));
            admission::issue_for_tests(proof.execution().clone(), episode, &token).unwrap();
            polls(20).await;
            assert!(io.posts.to(CANARY_CHANNEL).is_empty(), "before due, both local and exact must wait");
            polls(20).await;
            let posts = io.posts.to(CANARY_CHANNEL);
            if retired.is_none() {
                assert_eq!(posts, ["local saved payload", "following exact", "-# ✅ 완료"], "due Budget must drain even in verified mode");
                let store = harness.channel();
                assert_eq!(store.ledger().piece(1).unwrap().local_retry, Some(crate::services::tui_o::store::ledger::LocalRetryMark::Attempt { root_serial: 0 }));
                assert!(matches!(store.ledger().piece(1).unwrap().outcome, Some(PieceOutcome::Posted(_))));
                assert_eq!(store.ledger().piece(2).unwrap().local_retry, None);
            } else {
                assert!(posts.is_empty(), "disallowed source remains held, including retired source");
                assert_eq!(harness.channel().ledger().next_serial(), 1);
            }
            hosts.pop().unwrap().stop_and_join().await.unwrap();
            admission::set_ready_for_tests(CANARY_CHANNEL, false);
            admission::forget_channel_for_tests(CANARY_CHANNEL);
        }
    });
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(None);
}

#[test]
fn local_retry_retired_source_stays_held_when_only_the_new_source_is_allowed() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::local_retry_retired_source_stays_held_when_only_the_new_source_is_allowed"
    )) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _fake_tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (context, canonical, retained) = durable_canary_without_launch_files(root.path());
    std::fs::hard_link(&retained, &canonical).unwrap();
    crate::services::discord::runtime_store::fsync_parent_dir(&canonical).unwrap();
    publish_nonce(&context);
    dedupe::register_tmux_channel(CANARY_TMUX, CANARY_CHANNEL);
    dedupe::set_codex_delivery_permission_for_tests(
        &context,
        dedupe::CodexDeliveryPermissionForTests::Allowed,
    );
    dedupe::resolve_codex_claims();
    let (harness, _, source) = switched_over_on(CANARY_CHANNEL, b"");
    assert!(dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &source
    ));
    let mut store = harness.channel();
    let mut key = unit("old");
    key.channel_id = CANARY_CHANNEL;
    key.provider = ShadowProvider::Codex;
    store
        .append_ledger(LedgerEntry::Prepared {
            notice_origin: None,
            serial: 0,
            unit_key: key,
            piece_index: 0,
            payload: "held old source".into(),
            anchor_id: 100,
            epoch: 1,
            local_retry: Some(crate::services::tui_o::store::ledger::LocalRetryMark::Budget),
        })
        .unwrap();
    store
        .append_ledger(LedgerEntry::NotFound { serial: 0 })
        .unwrap();
    store.set_retired(&source, true).unwrap();
    let current_path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{ID}.jsonl"));
    let current = source_id_for(ID, &current_path).unwrap();
    assert!(dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &current
    ));
    store.attach_source(&current).unwrap();
    drop(store);
    harness.gate.acquired();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
        .block_on(async {
            let bindings = Arc::new(FakeBindings::new());
            let mut event = bound(
                1,
                None,
                BindingTarget::Source(current.clone()),
                BindingCause::Startup,
                None,
            );
            event.channel_id = CANARY_CHANNEL;
            event.provider = ShadowProvider::Codex;
            event.tmux_session = CANARY_TMUX.into();
            event.execution_nonce = context.execution_nonce.clone();
            bindings.commit(event);
            let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Codex, bindings);
            polls(70).await;
            assert!(
                !task.is_finished(),
                "actor stopped: {:?}",
                harness.alarms.taken()
            );
            assert!(dedupe::codex_verified_channel_delivery_allowed(
                CANARY_CHANNEL
            ));
            assert!(dedupe::codex_verified_o_source_allowed(
                CANARY_CHANNEL,
                &current
            ));
            assert!(!dedupe::codex_verified_o_source_allowed(
                CANARY_CHANNEL,
                &source
            ));
            assert_eq!(
                harness.port.posts(),
                Vec::<String>::new(),
                "a pending retry must pin the old source policy"
            );
            assert_eq!(harness.channel().ledger().next_serial(), 1);
            halt(stop, task).await;
        });
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(None);
}

#[test]
fn operator_resume_verified_source_hold_preserves_approval_cursor_and_post_count() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::operator_resume_verified_source_hold_preserves_approval_cursor_and_post_count"
    )) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _fake_tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (context, canonical, retained) = durable_canary_without_launch_files(root.path());
    let (harness, path, source) = switched_over_on(CANARY_CHANNEL, b"");
    let mut channel = harness.channel();
    let mut key = unit("approved");
    key.channel_id = CANARY_CHANNEL;
    key.provider = ShadowProvider::Codex;
    channel
        .append_ledger(LedgerEntry::Prepared {
            notice_origin: None,
            local_retry: None,
            serial: 0,
            unit_key: key,
            piece_index: 0,
            payload: "held output".into(),
            anchor_id: 100,
            epoch: 1,
        })
        .unwrap();
    channel
        .append_ledger(LedgerEntry::Rejected {
            serial: 0,
            status: 403,
        })
        .unwrap();
    harness
        .store
        .record_operator_resume(CANARY_CHANNEL, 0, "operator", "restored")
        .unwrap();
    let cursor = channel.cursor(&source).unwrap().clone();
    harness.gate.acquired();
    for source_only in [false, true] {
        if source_only {
            std::fs::hard_link(&retained, &canonical).unwrap();
            crate::services::discord::runtime_store::fsync_parent_dir(&canonical).unwrap();
            publish_nonce(&context);
            dedupe::register_tmux_channel(CANARY_TMUX, CANARY_CHANNEL);
            dedupe::set_codex_delivery_permission_for_tests(
                &context,
                dedupe::CodexDeliveryPermissionForTests::Allowed,
            );
            dedupe::resolve_codex_claims();
            assert!(dedupe::runtime_binding_for_tmux_session(CANARY_TMUX).is_some());
        }
        assert_eq!(
            dedupe::codex_verified_channel_delivery_allowed(CANARY_CHANNEL),
            source_only
        );
        assert!(!dedupe::codex_verified_o_source_allowed(
            CANARY_CHANNEL,
            &source
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                let (stop, task) = spawn_as(harness.writer(), ShadowProvider::Codex);
                append(
                    &path,
                    &output_line(
                        "response_item",
                        serde_json::json!({"type":"message","role":"assistant",
            "id":"new","content":[{"type":"output_text","text":"following"}]}),
                    ),
                );
                polls(3).await;
                assert!(
                    !task.is_finished(),
                    "the live actor remains held by its real source guard"
                );
                assert!(harness.port.posts().is_empty());
                assert_eq!(
                    harness
                        .channel()
                        .ledger()
                        .approval(0)
                        .unwrap()
                        .consumed_serial,
                    None
                );
                assert_eq!(harness.channel().ledger().next_serial(), 1);
                assert_eq!(harness.channel().cursor(&source), Some(&cursor));
                halt(stop, task).await;
            });
    }
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(None);
}
