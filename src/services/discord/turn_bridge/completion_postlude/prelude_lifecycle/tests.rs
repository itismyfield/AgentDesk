use super::*;

fn fixture(prompt: &str) -> (tempfile::TempDir, InflightTurnState) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        731_430,
        None,
        77,
        731_431,
        731_432,
        "raw".into(),
        None,
        None,
        Some(path.to_string_lossy().into_owned()),
        None,
        0,
    );
    state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    state.started_at = "2026-10-24T00:00:00Z".into();
    state.followup_reply_context = Some(format!(
        "\u{1e}agentdesk.retry-prompt.v1\n{}",
        serde_json::json!({
            "version": 1, "legacy_rendered": false, "raw": null,
            "selected": {"recovery": null, "feedback": null, "wip": null},
            "expected_prompt": prompt,
        })
    ));
    (root, state)
}

#[test]
fn no_submit_notice_and_foreign_user_record_do_not_generate_next_wip() {
    let (_root, state) = fixture("exact provider prompt");
    assert!(!confirmed_input(&state), "NO_SUBMIT_NO_NEXT_WIP");
    let path = state.output_path.as_ref().unwrap();
    for record in [
        serde_json::json!({"type":"assistant", "timestamp":"2026-10-24T00:00:01Z", "message":{"content":"exact provider prompt"}}),
        serde_json::json!({"type":"user", "isMeta":true, "timestamp":"2026-10-24T00:00:01Z", "message":{"content":"exact provider prompt"}}),
        serde_json::json!({"type":"user", "timestamp":"2026-10-23T23:59:59Z", "message":{"content":"exact provider prompt"}}),
        serde_json::json!({"type":"user", "timestamp":"2026-10-24T00:00:01Z", "message":{"content":"different input"}}),
        serde_json::json!({"type":"user", "timestamp":"2026-10-24T00:00:01Z", "message":{"content":[{"type":"text","text":"exact provider prompt"},{"type":"image"}]}}),
    ] {
        std::fs::write(path, format!("{record}\n")).unwrap();
        assert!(
            !confirmed_input(&state),
            "EXACT_CURRENT_USER_RECORD_REQUIRED record={record}"
        );
    }
}

#[test]
fn native_receipt_requires_current_offset_complete_line_and_exact_folded_bytes() {
    let (_root, mut state) = fixture("[User: Alice (ID: 77)]\nbody\r\n");
    let prompt = state
        .retry_prompt_snapshot()
        .unwrap()
        .expected_prompt
        .unwrap();
    let record = serde_json::json!({"type":"user", "timestamp":"2026-10-24T00:00:01Z", "uuid":"same-attempt", "message":{"role":"user", "content":[{"type":"text", "text":prompt}]}});
    let line = format!("{record}\n");
    let path = state.output_path.as_ref().unwrap().clone();
    std::fs::write(&path, line.trim_end_matches('\n')).unwrap();
    assert!(
        !confirmed_input(&state),
        "PARTIAL_NATIVE_RECORD_IS_NOT_RECEIPT"
    );
    std::fs::write(&path, &line).unwrap();
    assert!(confirmed_input(&state));
    state.turn_start_offset = Some(line.len() as u64);
    assert!(
        !confirmed_input(&state),
        "PRIOR_ATTEMPT_RECORD_IS_NOT_RECEIPT"
    );
    state.turn_start_offset = Some(0);
    let folded =
        format!("\n\n<pasted_content id=\"a1b2\">\n{prompt}\n</pasted_content id=\"a1b2\">\n");
    let record = serde_json::json!({"type":"user", "timestamp":"2026-10-24T00:00:01Z", "message":{"content":folded}});
    std::fs::write(&path, format!("{record}\n")).unwrap();
    assert!(confirmed_input(&state), "MEASURED_FOLDED_PASTE_EXACT_BYTES");
}

#[test]
fn process_completion_keeps_reminders_without_a_tui_receipt_snapshot() {
    let (_root, mut state) = fixture("headless input");
    state.followup_reply_context = None;
    state.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
    assert!(
        confirmed_input(&state),
        "PROCESS_COMPLETION_REMAINS_CONFIRMED"
    );
    state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    assert!(
        !confirmed_input(&state),
        "TUI_COMPLETION_STILL_NEEDS_RECEIPT"
    );
}
