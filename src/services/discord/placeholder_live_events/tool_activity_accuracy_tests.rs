use super::super::status_events::{
    status_events_from_json, status_events_from_tool_result_with_id,
    status_events_from_tool_use_with_id,
};
use super::*;
use serde_json::json;

fn start(ring: &mut ToolActivityRing, name: &str, id: &str) {
    for event in
        status_events_from_tool_use_with_id(name, r#"{"description":"audit source"}"#, Some(id))
    {
        ring.apply(&event, None);
    }
}

#[test]
fn accuracy_unknown_result_preserves_pending_and_completed_rows() {
    let mut ring = ToolActivityRing::default();
    start(&mut ring, "Read", "done");
    ring.apply(&observation(None, None, Some("done"), Some(true)), None);
    start(&mut ring, "Read", "active");
    for event in status_events_from_json(
        &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"not-in-ring","content":"PRIVATE_RESULT"}]}}),
    ) {
        ring.apply(&event, None);
    }
    let out = ring.render(6);
    assert!(
        out.contains("실행 1") && out.contains("▶ Read") && out.contains("✓ Read"),
        "{out}"
    );
    assert!(
        !out.contains('?') && !out.contains("PRIVATE_RESULT"),
        "{out}"
    );
    ring.apply(&observation(None, None, Some("active"), Some(false)), None);
    assert!(ring.render(6).contains("실패 1"));
    assert_eq!(
        ring.rows.iter().filter(|r| r.result == Some(true)).count(),
        1
    );
}

#[test]
fn accuracy_spawn_ack_does_not_finish_the_agent() {
    let mut ring = ToolActivityRing::default();
    start(&mut ring, "functions.spawn_agent", "spawn-call");
    for event in status_events_from_tool_result_with_id(
        Some("functions.spawn_agent"),
        false,
        Some("spawn-call"),
    ) {
        ring.apply(&event, None);
    }
    let out = ring.render(6);
    assert!(out.contains("▶ Task") && !out.contains('✓'), "{out}");
    ring.apply(
        &StatusEvent::SubagentEnd {
            success: true,
            agent_id: Some("agent-1".into()),
            desc: None,
            tool_use_id: Some("spawn-call".into()),
            summary: None,
            ack_only: false,
        },
        None,
    );
    assert!(ring.render(6).contains("✓ Task"));
    start(&mut ring, "spawn_agent", "failed-launch");
    ring.apply(
        &observation(None, None, Some("failed-launch"), Some(false)),
        None,
    );
    assert!(ring.render(6).contains("✗ Task"));
}

#[test]
fn accuracy_panel_budget_keeps_plan_tasks_and_subagents_sections() {
    let sections = vec![
        "🔧 도구 1 · 실행 1 · 실패 0\n▶ Bash cargo check".into(),
        format!("Plan\n{}", "- [ ] 긴 계획\n".repeat(90)),
        format!("Tasks\n{}", "- 작업 진행\n".repeat(90)),
        format!("Subagents\n{}", "- 에이전트 진행\n".repeat(90)),
    ];
    let out = fit_panel(sections);
    for title in ["Plan", "Tasks", "Subagents"] {
        assert!(out.contains(title), "{title}: {out}");
    }
    assert!(
        out.len() <= 1400 && out.encode_utf16().count() <= 1936,
        "{out}"
    );
}

#[test]
fn accuracy_original_timestamp_survives_replay_and_sixty_second_clock_shift() {
    let _clock = super::super::test_clock::Guard::new();
    let value = json!({"type":"assistant", "timestamp":"2026-10-18T10:00:00Z", "message":{"content":[{"type":"tool_use","id":"stamp","name":"Bash","input":{"command":"cargo check"}}]}});
    let observe = || {
        let mut ring = ToolActivityRing::default();
        for event in status_events_from_json(&value) {
            ring.apply(&event, None);
        }
        ring.render(6)
    };
    let first = observe();
    assert!(first.contains("<t:1792317600:R>"), "{first}");
    let mut state = super::super::status_panel::StatusPanelState::default();
    super::super::test_clock::aged_stamp(&mut state, std::time::Duration::from_secs(60));
    assert_eq!(first, observe());
}

#[test]
fn accuracy_timestamp_defaults_clamps_and_enriches_partial_starts() {
    let _clock = super::super::test_clock::Guard::new();
    let old: StatusEvent = serde_json::from_value(
        json!({"ToolActivity":{"name":"Read", "input":null, "tool_use_id":"old", "success":null}}),
    )
    .unwrap();
    assert!(matches!(
        old,
        StatusEvent::ToolActivity {
            source_unix: None,
            ..
        }
    ));
    for stamp in [
        Value::Null,
        json!(0),
        json!(-1),
        json!("invalid"),
        json!(chrono::Utc::now().timestamp() + 3600),
    ] {
        let mut value = json!({"type":"assistant", "message":{"content":[{"type":"tool_use","id":"time","name":"Read","input":{"path":"src/lib.rs"}}]}});
        if !stamp.is_null() {
            value["timestamp"] = stamp;
        }
        let before = observed_unix();
        let mut ring = ToolActivityRing::default();
        for event in status_events_from_json(&value) {
            ring.apply(&event, None);
        }
        assert_eq!(ring.rows.len(), 1);
        let row = ring.rows.front().unwrap();
        assert!(
            row.started_unix >= before && row.started_unix <= observed_unix(),
            "{value}: {}",
            row.started_unix
        );
    }
    let mut ring = ToolActivityRing::default();
    start(&mut ring, "Read", "partial");
    for event in status_events_from_json(
        &json!({"type":"assistant", "timestamp":1792317600,"message":{"content":[{"type":"tool_use","id":"partial","name":"Read","input":{"path":"src/lib.rs"}}]}}),
    ) {
        ring.apply(&event, None);
    }
    assert_eq!(ring.rows.len(), 1);
    assert_eq!(ring.rows[0].started_unix, 1792317600);
}

#[test]
fn accuracy_panel_keeps_safe_headings_when_content_must_be_hidden() {
    let out = fit_panel(vec![
        "🔧 도구 1".into(),
        "Plan\n- [ ] env MYSQL PrivateSentinel".into(),
        "Tasks\n-----BEGIN PRIVATE KEY-----\nSmallKeyMaterial".into(),
    ]);
    assert!(out.contains("Plan") && out.contains("Tasks"), "{out}");
    assert!(
        !out.contains("PrivateSentinel") && !out.contains("SmallKeyMaterial"),
        "{out}"
    );
}
