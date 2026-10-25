use super::tests::fixture;
use super::*;

pub(super) fn indeterminate_cases(state: &InflightTurnState) -> Vec<InflightTurnState> {
    let mut cases = Vec::new();
    let mut changed = state.clone();
    changed.started_at = "unparseable".into();
    cases.push(changed);
    let mut changed = state.clone();
    changed.output_path = None;
    cases.push(changed);
    let mut changed = state.clone();
    changed.turn_start_offset = None;
    cases.push(changed);
    let mut changed = state.clone();
    changed.output_path = Some(format!("{}.missing", state.output_path.as_ref().unwrap()));
    cases.push(changed);
    let mut changed = state.clone();
    changed.turn_start_offset = Some(u64::MAX);
    cases.push(changed);
    let mut changed = state.clone();
    let mut context = changed.retry_prompt_snapshot().unwrap();
    context.expected_prompt = None;
    changed.followup_reply_context = context.encode();
    cases.push(changed);
    cases
}

#[test]
fn indeterminate_receipt_uses_normal_completion_but_negative_evidence_does_not() {
    let (_root, state) = fixture("exact prompt");
    for changed in indeterminate_cases(&state) {
        assert!(
            input_receipt(&changed).is_err(),
            "INDETERMINATE_RECEIPT_IS_DISTINCT"
        );
        assert!(
            confirmed_input(&changed),
            "INDETERMINATE_NORMAL_COMPLETION_FALLBACK"
        );
    }
    let wrong = serde_json::json!({"type":"user", "timestamp":chrono::Utc::now().to_rfc3339(), "message":{"content":"other prompt"}});
    std::fs::write(state.output_path.as_ref().unwrap(), format!("{wrong}\n")).unwrap();
    assert_eq!(
        input_receipt(&state),
        Ok(false),
        "NEGATIVE_EVIDENCE_IS_NOT_FALLBACK"
    );
    assert!(
        !confirmed_input(&state),
        "NEGATIVE_EVIDENCE_IS_NOT_FALLBACK"
    );
}

#[test]
fn codex_normalized_relay_falls_back_and_native_rollout_still_requires_exact_input() {
    let (_root, mut state) = fixture("exact prompt");
    state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    // This is the assistant event emitted by codex_rebind_stream_message_json.
    let normalized = serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text", "text":"done"}]}});
    std::fs::write(
        state.output_path.as_ref().unwrap(),
        format!("{normalized}\n"),
    )
    .unwrap();
    assert!(
        input_receipt(&state).is_err(),
        "CODEX_NORMALIZED_RELAY_IS_INDETERMINATE"
    );
    assert!(
        confirmed_input(&state),
        "CODEX_NORMALIZED_RELAY_COMPLETION_FALLBACK"
    );
    let native = serde_json::json!({"type":"event_msg", "timestamp":chrono::Utc::now().to_rfc3339(), "payload":{"type":"user_message", "message":"exact prompt"}});
    std::fs::write(state.output_path.as_ref().unwrap(), format!("{native}\n")).unwrap();
    assert_eq!(
        input_receipt(&state),
        Ok(true),
        "CODEX_NATIVE_ROLLOUT_RECEIPT"
    );
}
