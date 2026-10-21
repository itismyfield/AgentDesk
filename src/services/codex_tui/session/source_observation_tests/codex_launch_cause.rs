use super::*;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as events};

#[test]
fn a_replaced_rollout_defers_production_rehydration_without_consuming_startup() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (runtime, _guards) = binding_context::tests::fixture_after_shared_test_env_lock();
    dedupe::reset_state_for_tests();
    events::set_test_root(Some(runtime.path()));
    let sessions = tempfile::tempdir().unwrap();
    let tmux = format!("replaced-discovery-{}", uuid::Uuid::new_v4().simple());
    let prepared = PreparedIncarnation::prepare_pinned(
        "codex",
        &tmux,
        Some(42),
        None,
        false,
        Some(sessions.path().canonicalize().unwrap()),
        (Some(digest("replacement input")), Some("shadow".into())),
    )
    .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce"),
        &prepared.context.execution_nonce,
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let path = sessions.path().join(format!("rollout-test-{id}.jsonl"));
    let header = json!({"type":"session_meta","payload":{
        "id":id,"cwd":"/synthetic","source":"cli",
        "timestamp":(prepared.context.created_at + chrono::Duration::seconds(1)).to_rfc3339()
    }});
    let input = json!({"type":"response_item","payload":{
        "type":"message","role":"user",
        "content":[{"type":"input_text","text":"replacement input"}]
    }});
    let native = format!("{header}\n{input}\n");
    std::fs::write(&path, &native).unwrap();
    let target = path.clone();
    before_final_identity(move || {
        let replacement = target.with_extension("replacement");
        std::fs::write(&replacement, native).unwrap();
        std::fs::rename(replacement, target).unwrap();
    });
    let binding = dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(id),
        last_offset: 0,
        relay_last_offset: None,
    };
    assert!(!dedupe::register_rehydrated_tmux_runtime_binding(
        "codex",
        &tmux,
        42,
        binding.clone(),
    ));
    assert!(
        events::records_strict(42)
            .unwrap()
            .unwrap_or_default()
            .is_empty()
    );
    assert!(dedupe::peek_tmux_runtime_binding(&tmux).is_none());
    assert!(dedupe::register_rehydrated_tmux_runtime_binding(
        "codex", &tmux, 42, binding,
    ));
    let history = events::records_strict(42).unwrap().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].cause, events::BindingCause::Startup);
    assert_eq!(history[0].evidence.hook_event, None);
    events::set_test_root(None);
    dedupe::reset_state_for_tests();
}
