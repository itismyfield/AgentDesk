use super::*;
use crate::services::discord::placeholder_live_events::status_events::{
    status_events_from_json, status_events_from_tool_result_with_id,
    status_events_from_tool_use_with_id,
};
use crate::services::discord::placeholder_live_events::{
    PlaceholderLiveEvents, status_panel::StatusPanelState,
};
use crate::services::provider::ProviderKind;
use serde_json::json;

fn begin(state: &mut StatusPanelState, name: &str, input: Value, id: Option<&str>) {
    for event in status_events_from_tool_use_with_id(name, &input.to_string(), id) {
        state.apply(event);
    }
}
fn end(state: &mut StatusPanelState, name: &str, id: Option<&str>, error: bool) {
    for event in status_events_from_tool_result_with_id(Some(name), error, id) {
        state.apply(event);
    }
}
fn render(state: StatusPanelState) -> String {
    super::super::status_panel::render_status_panel(
        state,
        &ProviderKind::Claude,
        "턴 시작 : fixed\n마지막 업데이트 : fixed".to_string(),
        None,
    )
}

#[test]
fn activity_id_pairing_fifo_and_duplicates_never_mark_the_wrong_row() {
    let mut state = StatusPanelState::default();
    begin(
        &mut state,
        "Bash",
        json!({"command":"echo alpha"}),
        Some("a"),
    );
    begin(
        &mut state,
        "Read",
        json!({"file_path":"src/beta.rs"}),
        Some("b"),
    );
    end(&mut state, "Read", Some("b"), true);
    let out = render(state.clone());
    assert!(out.contains("▶ Bash  echo alpha"), "{out}");
    assert!(out.contains("✗ Read  src/beta.rs"), "{out}");
    end(&mut state, "Read", Some("b"), false);
    assert!(render(state.clone()).contains("▶ Bash  echo alpha"));
    end(&mut state, "Bash", Some("a"), false);
    assert!(render(state).contains("✓ Bash  echo alpha"));

    let mut fifo = StatusPanelState::default();
    begin(&mut fifo, "Read", json!({"file_path":"first.rs"}), None);
    begin(&mut fifo, "Read", json!({"file_path":"second.rs"}), None);
    end(&mut fifo, "Read", None, false);
    let out = render(fifo);
    assert!(
        out.contains("✓ Read  first.rs") && out.contains("▶ Read  second.rs"),
        "{out}"
    );
}

#[test]
fn activity_unmatched_result_and_ring_eviction_fail_closed() {
    let mut state = StatusPanelState::default();
    for i in 0..CAPACITY + 1 {
        begin(
            &mut state,
            "Read",
            json!({"file_path":format!("file{i}.rs")}),
            None,
        );
    }
    end(&mut state, "Read", None, false);
    assert!(!state.tool_activity.render(6).contains("✓"));
    end(&mut state, "Read", Some("never-observed"), false);
    assert!(!state.tool_activity.render(6).contains("✓"));
    state.apply(StatusEvent::TurnCompleted {
        background: false,
        background_agent_pending: false,
    });
    let out = state.tool_activity.render(6);
    assert!(out.contains("? Read"), "{out}");
    assert!(!out.contains("✓") && !out.contains("▶"), "{out}");
}

#[test]
fn activity_turn_and_session_boundaries_clear_the_ring_and_counts() {
    let mut state = StatusPanelState::default();
    begin(
        &mut state,
        "Bash",
        json!({"command":"echo previous"}),
        Some("a"),
    );
    state.reset_turn_content_preserving_unfinished_footer_residuals();
    assert!(!render(state.clone()).contains("previous"));
    begin(
        &mut state,
        "Read",
        json!({"file_path":"next.rs"}),
        Some("b"),
    );
    assert!(render(state.clone()).contains("도구 1"));
    state.reset_session_content();
    assert!(!render(state).contains("next.rs"));
    assert!(
        PlaceholderLiveEvents::default()
            .render_status_panel(ChannelId::new(7), &ProviderKind::Codex, 1_700_000_000)
            .contains("아직 없음")
    );
}

#[test]
fn activity_six_row_limit_counts_and_preserves_running_rows() {
    let mut state = StatusPanelState::default();
    begin(
        &mut state,
        "Bash",
        json!({"command":"echo active"}),
        Some("active"),
    );
    for i in 0..8 {
        let id = format!("r{i}");
        begin(
            &mut state,
            "Read",
            json!({"file_path":format!("file{i}.rs")}),
            Some(&id),
        );
        end(&mut state, "Read", Some(&id), false);
    }
    let out = render(state);
    assert!(
        out.contains("도구 9 · 실행 1 · 실패 0") && out.contains("… 외 3개"),
        "{out}"
    );
    assert!(out.contains("▶ Bash  echo active"), "{out}");
    assert_eq!(
        out.lines()
            .filter(|line| ["-# ✓ ", "-# ▶ "].iter().any(|p| line.starts_with(p)))
            .count(),
        6
    );
}

#[test]
fn activity_budget_drops_whole_old_rows_including_subtext_bytes() {
    let mut state = StatusPanelState::default();
    for i in 0..7 {
        let id = format!("r{i}");
        begin(
            &mut state,
            "Task",
            json!({"description":format!("ROW{i}{}END", "🦀한".repeat(40))}),
            Some(&id),
        );
        end(&mut state, "Task", Some(&id), false);
    }
    let out = render(state.clone());
    assert!(out.len() <= 1400, "{}: {out}", out.len());
    assert!(out.encode_utf16().count() <= 2000);
    assert!(out.contains("… 외 "));
    let full = state.tool_activity.render(6);
    for row in out.lines().filter(|l| l.starts_with("-# ✓ ")) {
        assert!(
            full.lines()
                .any(|original| original == row.trim_start_matches("-# ")),
            "partial row: {row}"
        );
    }
    let footer = super::super::super::single_message_panel::compose_footer_status_block("⠋", &out);
    assert!(footer.encode_utf16().count() <= 2000);
    assert_eq!(
        footer.lines().skip(1).collect::<Vec<_>>().join("\n").len(),
        out.lines().skip(1).collect::<Vec<_>>().join("\n").len()
    );
}

#[test]
fn activity_claude_and_codex_json_paths_share_safe_rows_and_result_identity() {
    let store = PlaceholderLiveEvents::default();
    let channel = ChannelId::new(12);
    store.push_status_events(channel, status_events_from_json(&json!({"type":"assistant","cwd":"/work/project","message":{"content":[
        {"type":"tool_use","id":"a","name":"exec_command","input":{"cmd":"cargo test"}},
        {"type":"tool_use","id":"b","name":"Read","input":{"file_path":"/work/project/src/lib.rs"}}
    ]}})));
    store.push_status_events(channel, status_events_from_json(&json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"b","is_error":true,"content":"RESULT_MUST_NEVER_APPEAR"}
    ]}})));
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let out = store.render_status_panel(channel, &provider, 1_700_000_000);
        assert!(
            out.contains("▶ Bash  cargo test") && out.contains("✗ Read  src/lib.rs"),
            "{out}"
        );
        assert!(!out.contains("RESULT_MUST_NEVER_APPEAR"));
    }
}

#[test]
fn activity_class_switch_is_byte_identical_to_existing_static_renderer() {
    let mut state = StatusPanelState::default();
    begin(
        &mut state,
        "Bash",
        json!({"command":"echo details"}),
        Some("a"),
    );
    state.tool_activity_display = ToolActivityDisplay::Class;
    let class = render(state.clone());
    state.tool_activity = Default::default();
    let legacy = render(state);
    assert_eq!(class.as_bytes(), legacy.as_bytes());
    assert!(class.contains("마지막 도구 ([Bash])"));
    assert!(!class.contains("details"));
    let store = PlaceholderLiveEvents::with_tool_activity_display(ToolActivityDisplay::Class);
    store.push_status_events(
        ChannelId::new(13),
        status_events_from_tool_use_with_id("Read", r#"{"file_path":"hidden.rs"}"#, Some("a")),
    );
    let out = store.render_status_panel(ChannelId::new(13), &ProviderKind::Claude, 1_700_000_000);
    assert!(out.contains("마지막 도구 ([Read])") && !out.contains("hidden.rs"));
}

#[test]
fn activity_fake_render_example() {
    let mut state = StatusPanelState::default();
    state.apply(StatusEvent::ToolActivityCwd {
        cwd: "/work/agentdesk".into(),
    });
    begin(
        &mut state,
        "Read",
        json!({"file_path":"/work/agentdesk/src/lib.rs"}),
        Some("r"),
    );
    end(&mut state, "Read", Some("r"), false);
    begin(
        &mut state,
        "Edit",
        json!({"file_path":"src/main.rs","old_string":"old","new_string":"new\nline"}),
        Some("e"),
    );
    end(&mut state, "Edit", Some("e"), false);
    begin(
        &mut state,
        "Bash",
        json!({"command":"npm run build"}),
        Some("b"),
    );
    end(&mut state, "Bash", Some("b"), true);
    begin(
        &mut state,
        "mcp__memento__recall",
        json!({"secret":"NEVER_DISPLAYED"}),
        Some("m"),
    );
    let output = render(state);
    assert!(output.contains("✓ Read  src/lib.rs") && output.contains("✗ Bash  npm run build"));
    assert!(output.contains("memento · recall") && !output.contains("NEVER_DISPLAYED"));
    println!("{output}");
}

#[test]
fn activity_whole_panel_does_not_repeat_secret_descriptions_in_other_sections() {
    for name in ["Task", "Agent", "Bash", "exec_command", "WebSearch"] {
        let mut state = StatusPanelState::default();
        begin(
            &mut state,
            name,
            json!({"description":"check ghp_ShortFakeLeak9", "command":"echo --password 'FLAG_HEAD FLAG_TAIL'", "cmd":"curl https://SHORTUSER:SHORTPASS@example.test", "query":"api_key=SHORTQUERY"}),
            Some("s"),
        );
        let out = render(state);
        for forbidden in [
            "ghp_ShortFakeLeak9",
            "FLAG_HEAD",
            "FLAG_TAIL",
            "SHORTUSER",
            "SHORTPASS",
            "SHORTQUERY",
        ] {
            assert!(!out.contains(forbidden), "{name}: {forbidden}: {out}");
        }
    }
}

#[test]
fn activity_partial_tui_start_is_enriched_without_double_counting() {
    let mut state = StatusPanelState::default();
    for value in [
        json!({"type":"content_block_start","content_block":{"type":"tool_use","id":"partial","name":"Bash","input":{}}}),
        json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"partial","name":"Bash","input":{"command":"cargo test"}}]}}),
    ] {
        for event in status_events_from_json(&value) {
            state.apply(event);
        }
    }
    let out = render(state);
    assert!(
        out.contains("도구 1") && out.contains("▶ Bash  cargo test"),
        "{out}"
    );
}
