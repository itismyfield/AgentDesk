use super::*;

async fn wait_posts(harness: &Harness, task: &Actor, expected: usize) {
    for _ in 0..20 {
        polls(1).await;
        if harness.port.posts().len() >= expected || task.is_finished() {
            break;
        }
    }
    assert_eq!(
        harness.port.posts().len(),
        expected,
        "{:?}",
        harness.alarms.taken()
    );
}

async fn resumed_summary(provider: ShadowProvider, channel: u64) {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let (harness, path, source) = switched_over_on(channel, b"");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        harness._runtime.path(),
    );
    crate::config_live_reload::install(crate::config::Config {
        tui_o: Some(TuiOConfig::default()),
        ..crate::config::Config::default()
    });
    admission::set_ready_for_tests(channel, false);
    let bindings = Arc::new(FakeBindings::new());
    let mut startup = bound(
        1,
        None,
        BindingTarget::Source(source),
        BindingCause::Startup,
        None,
    );
    startup.channel_id = channel;
    startup.provider = provider;
    bindings.commit(startup);
    harness.gate.acquired();
    let (start, body, end, first) = match provider {
        ShadowProvider::Claude => (
            serde_json::json!({"type":"user","timestamp":"2026-10-19T00:00:00Z","message":{"content":"question"}}),
            serde_json::json!({"type":"assistant","apiBlockIndex":0,"message":{"id":"summary-body","content":[{"type":"text","text":"answer"}]}}),
            serde_json::json!({"type":"system","subtype":"stop_hook_summary","timestamp":"2026-10-19T00:00:07Z"}),
            "summary-body:0",
        ),
        ShadowProvider::Codex => (
            serde_json::json!({"type":"event_msg","timestamp":"2026-10-19T00:00:00Z","payload":{"type":"task_started","turn_id":"summary-turn"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"summary-body","content":[{"type":"output_text","text":"answer"}]}}),
            serde_json::json!({"type":"event_msg","timestamp":"2026-10-19T00:00:07Z","payload":{"type":"task_complete","turn_id":"summary-turn"}}),
            "summary-body",
        ),
    };
    let (stop, task) = spawn_with(harness.writer(), provider, Arc::clone(&bindings));
    append(&path, &native(start));
    append(&path, &native(body));
    wait_posts(&harness, &task, 1).await;
    assert!(
        harness
            .channel()
            .ledger()
            .summary_turn_marks()
            .contains(&format!("{first}#turn"))
    );
    halt(stop, task).await;
    let (stop, task) = spawn_with(harness.writer(), provider, Arc::clone(&bindings));
    append(&path, &native(end.clone()));
    wait_posts(&harness, &task, 2).await;
    append(&path, &native(end));
    polls(2).await;
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), ["answer", "-# ✅ 7초"]);
    let key = UnitKey {
        channel_id: channel,
        provider,
        kind: UnitKind::Tool,
        native_key: format!("{first}#turn_end"),
    };
    let store = harness.channel();
    let ledger = store.ledger();
    let (_, piece) = ledger.latest_piece(&key, 0).expect("one durable summary");
    assert!(matches!(piece.outcome, Some(PieceOutcome::Posted(_))));
    assert_eq!(ledger.next_serial(), 2);
    drop(store);
    let (stop, task) = spawn_with(harness.writer(), provider, bindings);
    polls(3).await;
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), ["answer", "-# ✅ 7초"]);
}

#[tokio::test(start_paused = true)]
async fn production_actors_resume_a_marked_turn_after_body_posted_and_emit_one_summary() {
    resumed_summary(ShadowProvider::Claude, 585_801).await;
    resumed_summary(ShadowProvider::Codex, 585_802).await;
}
