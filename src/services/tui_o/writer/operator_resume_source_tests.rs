use super::*;

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
