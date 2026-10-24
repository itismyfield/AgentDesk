use super::*;
use crate::services::discord::router::response_format::retry_prompt_context::Preludes;
use crate::services::discord::turn_bridge::recovery_text;
use crate::services::discord::{inflight, make_shared_data_for_tests_with_storage};
use crate::services::turn_orchestrator::{load_channel_pending_queue_checked, save_channel_queue};

fn queued(context: Option<String>, text: &str) -> Intervention {
    build_race_requeued_intervention(
        UserId::new(77),
        MessageId::new(731_401),
        text,
        true,
        context,
        false,
        false,
        Vec::new(),
        None,
    )
}

#[tokio::test]
async fn borrowed_preludes_survive_six_requeues_disk_restart_and_first_inflight_save_pg() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "prelude_seven",
        "prelude borrowing and durable handback",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(731_400);
    let raw = "quoted\r\n[WIP 리마인더] is user text\n";
    let body = "original user input\r\nwith exact bytes";
    recovery_text::store_turn_end_wip_warning(Some(&pool), &provider, channel.get(), "dirty")
        .unwrap();
    recovery_text::store_voluntary_feedback_reminder(
        Some(&pool),
        &provider,
        channel.get(),
        "evaluate",
    )
    .unwrap();
    recovery_text::store_session_retry_context_with_audit(
        Some(&pool),
        channel.get(),
        "user: recover",
        None,
    )
    .unwrap();
    let mut context = RetryPromptContext::raw(Some(raw.into()));
    let expected = "[WIP 리마인더 — 이전 턴 종료 시 커밋되지 않은 변경사항 감지]\n\ndirty\n\n[메모리 리마인더 — 이전 턴 recall 후 tool_feedback 미평가]\n\nevaluate\n\n[이전 대화 복원 — 새 세션 시작으로 최근 대화를 컨텍스트에 포함합니다]\n\nuser: recover\n\nquoted\r\n[WIP 리마인더] is user text\n";
    for attempt in 0..7 {
        let prepared = prepare(&shared, &provider, channel, context).await;
        assert_eq!(
            prepared.rendered.as_deref(),
            Some(expected),
            "SEVEN_ATTEMPTS_PROMPT_BYTES attempt={attempt}"
        );
        assert_eq!(prepared.context.raw.as_deref(), Some(raw));
        let prompt = TurnContext {
            provider: &provider,
            session_id: None,
            uploads: &[],
            trigger: None,
            reply: prepared.rendered.as_deref(),
            recent: None,
            knowledge: None,
            author_name: "Alice",
            author_id: UserId::new(77),
            user_input: body.into(),
        }
        .build();
        let expected_prompt =
            format!("{expected}\n\n[User: Alice (ID: 77)]\noriginal user input\nwith exact bytes");
        assert_eq!(
            prompt.as_bytes(),
            expected_prompt.as_bytes(),
            "ACTUAL_PROVIDER_PROMPT_BYTE_CONTRACT attempt={attempt}"
        );
        for (kind, value) in [
            ("turn_end_wip_warning", "dirty"),
            ("voluntary_feedback_reminder", "evaluate"),
            ("session_retry_context", "user: recover"),
        ] {
            assert_eq!(
                store::peek(Some(&pool), &store::key(kind, &provider, channel.get())).as_deref(),
                Some(value),
                "PRE_INFLIGHT_DURABLE_OWNER attempt={attempt}"
            );
        }
        let requeued = if attempt % 2 == 0 {
            // Early-busy and enqueue failure have not removed anything from the channel stash.
            prepared.context.requeue()
        } else {
            let mut state = InflightTurnState::new(
                provider.clone(),
                channel.get(),
                None,
                77,
                731_401,
                731_402,
                body.into(),
                None,
                None,
                None,
                None,
                0,
            );
            state.followup_reply_context = prepared.snapshot(&prompt);
            inflight::save_inflight_state(&state).unwrap();
            let restored = inflight::load_inflight_state(&provider, channel.get()).unwrap();
            let snapshot = RetryPromptContext::restore(restored.followup_reply_context.clone());
            assert_eq!(
                snapshot.raw.as_deref(),
                Some(raw),
                "INFLIGHT_STORES_RAW_NOT_RENDERED"
            );
            assert_eq!(snapshot.selected.wip.as_deref(), Some("dirty"));
            restored.followup_reply_context
        };
        let item = queued(requeued, body);
        save_channel_queue(&provider, "prelude-tests", channel, &[item], None).unwrap();
        let (restored, _) =
            load_channel_pending_queue_checked(&provider, "prelude-tests", channel).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].text, body);
        context = ingress(restored[0].reply_context.clone(), true, false);
    }
    let context = prepare(&shared, &provider, channel, context).await.context;
    assert_ne!(context.selected, Preludes::default());
    store::consume_confirmed(
        Some(&pool),
        &provider,
        channel.get(),
        "confirmed-turn",
        &context,
    )
    .await
    .unwrap();
    assert_eq!(
        store::peek(
            Some(&pool),
            &store::key("turn_end_wip_warning", &provider, channel.get())
        ),
        None
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn identical_legacy_and_raw_bytes_keep_distinct_semantics_through_real_queue_pg() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "prelude_provenance",
        "raw and legacy queue compatibility",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let provider = ProviderKind::Claude;
    let bytes = "[WIP 리마인더] same raw and legacy bytes\r\n";
    for (offset, value, raw) in [
        (0, Some(bytes.into()), false),
        (
            1,
            RetryPromptContext::raw(Some(bytes.into())).encode(),
            true,
        ),
    ] {
        let channel = ChannelId::new(731_410 + offset);
        recovery_text::store_turn_end_wip_warning(
            Some(&pool),
            &provider,
            channel.get(),
            "one new WIP",
        )
        .unwrap();
        save_channel_queue(
            &provider,
            "prelude-tests",
            channel,
            &[queued(value, "user")],
            None,
        )
        .unwrap();
        let (restored, _) =
            load_channel_pending_queue_checked(&provider, "prelude-tests", channel).unwrap();
        let context = ingress(restored[0].reply_context.clone(), true, false);
        assert_eq!(
            context.may_select(),
            raw,
            "REAL_QUEUE_RAW_LEGACY_PROVENANCE"
        );
        let prepared = prepare(&shared, &provider, channel, context).await;
        assert_eq!(prepared.context.raw.as_deref(), Some(bytes));
        if raw {
            assert_eq!(prepared.rendered.as_deref(), Some(format!("[WIP 리마인더 — 이전 턴 종료 시 커밋되지 않은 변경사항 감지]\n\none new WIP\n\n{bytes}").as_str()));
        } else {
            assert_eq!(
                prepared.rendered.as_deref(),
                Some(bytes),
                "LEGACY_ADDITIONAL_DECORATION_ZERO"
            );
        }
        let again = ingress(prepared.context.requeue(), true, false);
        assert_eq!(again.may_select(), raw);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn failed_raw_enqueue_and_newer_stash_do_not_lose_borrowed_preludes_pg() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "prelude_failure",
        "prelude enqueue failure and compare-delete race",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(731_420);
    recovery_text::store_turn_end_wip_warning(Some(&pool), &provider, channel.get(), "old")
        .unwrap();
    let prepared = prepare(&shared, &provider, channel, RetryPromptContext::raw(None)).await;
    let pending = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    std::fs::create_dir_all(pending.parent().unwrap()).unwrap();
    std::fs::write(&pending, "blocked queue directory").unwrap();
    assert!(
        save_channel_queue(
            &provider,
            "prelude-tests",
            channel,
            &[queued(prepared.context.requeue(), "input")],
            None
        )
        .is_err()
    );
    let key = store::key("turn_end_wip_warning", &provider, channel.get());
    assert_eq!(
        store::peek(Some(&pool), &key).as_deref(),
        Some("old"),
        "ENQUEUE_FAILURE_STASH_OWNER"
    );
    recovery_text::store_turn_end_wip_warning(Some(&pool), &provider, channel.get(), "newer")
        .unwrap();
    store::consume_confirmed(
        Some(&pool),
        &provider,
        channel.get(),
        "confirmed",
        &prepared.context,
    )
    .await
    .unwrap();
    assert_eq!(
        store::peek(Some(&pool), &key).as_deref(),
        Some("newer"),
        "COMPARE_DELETE_PRESERVES_NEWER_STASH"
    );
    assert!(
        store::consume_confirmed(
            None,
            &provider,
            channel.get(),
            "confirmed",
            &prepared.context
        )
        .await
        .is_err()
    );
    assert_eq!(
        store::peek(Some(&pool), &key).as_deref(),
        Some("newer"),
        "RETURN_NOT_NEEDED_WHEN_OWNER_NEVER_REMOVED"
    );
    pool.close().await;
    db.drop().await;
}

#[test]
fn durable_worker_and_exact_retry_preserve_legacy_rendered_bytes() {
    let rendered = "[WIP 리마인더] legacy rendered context";
    let legacy = ingress(Some(rendered.into()), false, true);
    assert!(!legacy.may_select(), "WORKER_LEGACY_DURABLE_PROVENANCE");
    assert_eq!(
        legacy.render_with(merge_reply_contexts).as_deref(),
        Some(rendered)
    );
    for queued in [false, true] {
        let wire = RetryPromptContext::for_routing(Some(rendered.into()), queued);
        let restored = ingress(wire, false, true);
        assert_eq!(
            restored.may_select(),
            !queued,
            "ROUTER_TO_WORKER_PROVENANCE"
        );
        assert_eq!(restored.raw.as_deref(), Some(rendered));
    }
    let raw = RetryPromptContext::raw(Some(rendered.into()));
    let restored = ingress(raw.requeue(), false, true);
    assert!(restored.may_select(), "WORKER_RAW_DURABLE_PROVENANCE");
    assert_eq!(restored.raw.as_deref(), Some(rendered));
}
