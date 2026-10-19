use super::*;
use crate::services::tui_o::shadow::tap::TuiOConfig;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::admission;
#[path = "tool_card_actor_replay_tests.rs"]
mod replay;
#[path = "turn_summary_actor_tests.rs"]
mod summaries;

fn native(value: serde_json::Value) -> Vec<u8> {
    format!("{value}\n").into_bytes()
}

fn native_records(provider: ShadowProvider) -> (Vec<Vec<u8>>, &'static str) {
    match provider {
        ShadowProvider::Claude => (
            vec![
                native(serde_json::json!({
                    "type":"assistant", "uuid":"uuid-tool", "apiBlockIndex":0,
                    "cwd":"/work/project", "message":{"id":"m-tool", "content":[{
                        "type":"tool_use", "id":"tool-call", "name":"Bash",
                        "input":{"command":"cargo check"},
                    }]},
                })),
                native(serde_json::json!({
                    "type":"user", "uuid":"uuid-result", "message":{"content":[{
                        "type":"tool_result", "tool_use_id":"tool-call",
                        "is_error":false, "content":"PRIVATE_TOOL_RESULT",
                    }]},
                })),
                row("m-body", "answer"),
            ],
            "m-tool:0",
        ),
        ShadowProvider::Codex => (
            vec![
                native(serde_json::json!({"type":"event_msg", "payload":{
                    "type":"task_started", "turn_id":"turn-1",
                }})),
                native(serde_json::json!({"type":"response_item", "payload":{
                    "type":"function_call", "id":"fc-tool", "call_id":"tool-call",
                    "name":"functions.exec_command", "arguments":"{\"cmd\":\"cargo check\"}",
                }})),
                native(serde_json::json!({"type":"response_item", "payload":{
                    "type":"function_call_output", "call_id":"tool-call",
                    "output":"PRIVATE_TOOL_RESULT",
                }})),
                native(serde_json::json!({"type":"response_item", "payload":{
                    "type":"message", "role":"assistant", "id":"m-body",
                    "content":[{"type":"output_text", "text":"answer"}],
                }})),
                native(serde_json::json!({"type":"event_msg", "payload":{
                    "type":"task_complete", "turn_id":"turn-1",
                }})),
            ],
            "fc-tool",
        ),
    }
}

async fn default_actor_posts_card_before_body(provider: ShadowProvider, channel: u64) {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let (harness, path, source) = switched_over_on(channel, b"");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        harness._runtime.path(),
    );
    let config = crate::config::Config {
        tui_o: Some(TuiOConfig::default()),
        ..crate::config::Config::default()
    };
    crate::config_live_reload::install(config);
    assert!(crate::services::tui_o::store::tool_card_switch::global_enabled());
    admission::set_ready_for_tests(channel, false);
    assert!(
        !admission::owed_delivery_ready(channel),
        "use production FIFO readiness"
    );
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
    let (stop, task) = spawn_with(harness.writer(), provider, Arc::clone(&bindings));
    let (records, native_key) = native_records(provider);
    for record in records {
        append(&path, &record);
    }
    let expected = if provider == ShadowProvider::Codex {
        3
    } else {
        2
    };
    for _ in 0..10 {
        polls(1).await;
        if harness.port.posts().len() >= expected || task.is_finished() {
            break;
        }
    }
    let posts = harness.port.posts();
    assert_eq!(
        posts.len(),
        expected,
        "{provider:?}: {posts:?}; {:?}",
        harness.alarms.taken()
    );
    assert_eq!(
        posts[0],
        if provider == ShadowProvider::Codex {
            "-# ▸ Bash  cargo check  ·"
        } else {
            "-# ▸ Bash  cargo check  ✓"
        }
    );
    assert_eq!(posts[1], "answer");
    if provider == ShadowProvider::Codex {
        assert_eq!(posts[2], "-# ✅ 완료");
    }
    assert!(
        !posts
            .iter()
            .any(|post| post.contains("PRIVATE_TOOL_RESULT"))
    );
    assert_eq!(
        *harness.port.prepared_before_post.lock().unwrap(),
        vec![true; expected]
    );
    halt(stop, task).await;
    let store = harness.channel();
    let native_tool = UnitKey {
        channel_id: channel,
        provider,
        native_key: native_key.into(),
        kind: UnitKind::Tool,
    };
    assert_eq!(
        store.ledger().excluded(&native_tool),
        Some(pieces::TOOL_CALL_CARD)
    );
    let card_key = UnitKey {
        native_key: format!("{native_key}#card"),
        ..native_tool
    };
    let ledger = store.ledger();
    assert_eq!(ledger.next_serial(), expected as u64);
    assert_eq!(ledger.piece(0).unwrap().unit_key, card_key);
    assert_eq!(ledger.piece(0).unwrap().payload, posts[0]);
    assert!(matches!(
        ledger.piece(0).unwrap().outcome,
        Some(PieceOutcome::Posted(_))
    ));
    assert_eq!(ledger.piece(1).unwrap().payload, "answer");
    assert_eq!(ledger.violation(), None);
    drop(store);
    let (stop, task) = spawn_with(harness.writer(), provider, bindings);
    polls(3).await;
    halt(stop, task).await;
    assert_eq!(
        harness.port.posts(),
        posts,
        "startup replay must not duplicate either post"
    );
    assert!(harness.alarms.taken().is_empty());
    assert!(
        !harness._runtime.path().join("o_tool_card_switch").exists(),
        "default-on startup must not write a preference"
    );
}

#[tokio::test(start_paused = true)]
async fn claude_actor_startup_defaults_to_card_then_body_through_production_fifo() {
    default_actor_posts_card_before_body(ShadowProvider::Claude, 584_641).await;
}

#[tokio::test(start_paused = true)]
async fn codex_actor_startup_defaults_to_card_then_body_through_production_fifo() {
    default_actor_posts_card_before_body(ShadowProvider::Codex, 584_642).await;
}
