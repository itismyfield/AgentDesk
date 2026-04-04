use super::completion_guard::{
    build_verdict_payload, extract_explicit_review_verdict, extract_review_decision,
};
use super::recovery_text::{
    clear_local_session_state, contains_stale_resume_error_text, handle_gemini_retry_boundary,
    output_file_has_stale_resume_error_after_offset, reset_gemini_retry_attempt_state,
    resolve_done_response, result_event_has_stale_resume_error,
    should_reset_gemini_retry_attempt_state, stream_error_requires_terminal_session_reset,
};
use super::tmux_runtime::{
    persisted_context_tokens, should_resume_watcher_after_turn, total_context_tokens,
};
use crate::services::discord::InflightTurnState;
use crate::services::provider::ProviderKind;
use std::io::Write;

#[test]
fn chained_batch_mid_turn_keeps_watcher_paused() {
    assert!(!should_resume_watcher_after_turn(true, false, false));
}

#[test]
fn locally_chainable_queue_keeps_watcher_paused() {
    assert!(!should_resume_watcher_after_turn(false, true, true));
}

#[test]
fn final_turn_without_remaining_queue_resumes_watcher() {
    assert!(should_resume_watcher_after_turn(false, false, true));
}

#[test]
fn persisted_context_tokens_uses_input_only() {
    // input_tokens represents full context window occupancy; output is excluded
    assert_eq!(persisted_context_tokens(610_000, 90_000), Some(610_000));
    assert_eq!(persisted_context_tokens(0, 0), None);
}

#[test]
fn total_context_tokens_saturates_on_overflow() {
    assert_eq!(total_context_tokens(u64::MAX, 1), u64::MAX);
}

#[test]
fn clear_local_session_state_drops_stale_resume_id_everywhere() {
    let mut new_session_id = Some("stale-session".to_string());
    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Claude,
        1479671298497183835,
        Some("adk-cc".to_string()),
        343742347365974026,
        1,
        2,
        "resume me".to_string(),
        Some("stale-session".to_string()),
        Some("AgentDesk-claude-adk-cc".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        0,
    );

    clear_local_session_state(&mut new_session_id, &mut inflight_state);

    assert_eq!(new_session_id, None);
    assert_eq!(inflight_state.session_id, None);
}

#[test]
fn stale_resume_text_helper_matches_known_error_shapes() {
    assert!(contains_stale_resume_error_text("Error: No conversation"));
    assert!(contains_stale_resume_error_text(
        "No conversation found for session"
    ));
    assert!(!contains_stale_resume_error_text(
        "The assistant explained why a conversation was missing context."
    ));
}

#[test]
fn terminal_session_reset_helper_matches_terminal_recovery_failures() {
    assert!(stream_error_requires_terminal_session_reset(
        "Gemini session could not be recovered after retry: Gemini stream ended without a terminal result",
        "",
    ));
    assert!(stream_error_requires_terminal_session_reset(
        "InvalidArgument: Gemini resume selector must be `latest` or a numeric session index",
        "",
    ));
    assert!(stream_error_requires_terminal_session_reset(
        "Qwen session could not be recovered after retry: Qwen stream ended without a terminal result",
        "",
    ));
    assert!(stream_error_requires_terminal_session_reset(
        "Qwen stream ended without a terminal result",
        "",
    ));
    assert!(!stream_error_requires_terminal_session_reset(
        "Gemini CLI not found",
        "",
    ));
}

#[test]
fn gemini_retry_reset_helper_requires_current_turn_partial_state() {
    assert!(should_reset_gemini_retry_attempt_state(
        "partial answer",
        None,
        false,
        false,
    ));
    assert!(should_reset_gemini_retry_attempt_state(
        "",
        Some("⚙ Bash: pwd"),
        true,
        false,
    ));
    assert!(!should_reset_gemini_retry_attempt_state(
        "", None, false, false,
    ));
}

#[test]
fn reset_gemini_retry_attempt_state_clears_partial_output_and_tool_flags() {
    let mut full_response = "partial answer".to_string();
    let mut current_tool_line = Some("⚙ Bash: pwd".to_string());
    let mut last_tool_name = Some("Bash".to_string());
    let mut last_tool_summary = Some("pwd".to_string());
    let mut any_tool_used = true;
    let mut has_post_tool_text = true;
    let mut response_sent_offset = 42usize;
    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Gemini,
        1479671298497183835,
        Some("adk-gm".to_string()),
        343742347365974026,
        1,
        2,
        "resume me".to_string(),
        Some("latest".to_string()),
        Some("AgentDesk-gemini-adk-gm".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        0,
    );
    inflight_state.full_response = full_response.clone();
    inflight_state.current_tool_line = current_tool_line.clone();
    inflight_state.any_tool_used = true;
    inflight_state.has_post_tool_text = true;
    inflight_state.response_sent_offset = response_sent_offset;

    reset_gemini_retry_attempt_state(
        &mut full_response,
        &mut current_tool_line,
        &mut last_tool_name,
        &mut last_tool_summary,
        &mut any_tool_used,
        &mut has_post_tool_text,
        &mut response_sent_offset,
        &mut inflight_state,
    );

    assert!(full_response.is_empty());
    assert_eq!(current_tool_line, None);
    assert_eq!(last_tool_name, None);
    assert_eq!(last_tool_summary, None);
    assert!(!any_tool_used);
    assert!(!has_post_tool_text);
    assert_eq!(response_sent_offset, 0);
    assert!(inflight_state.full_response.is_empty());
    assert_eq!(inflight_state.current_tool_line, None);
    assert!(!inflight_state.any_tool_used);
    assert!(!inflight_state.has_post_tool_text);
    assert_eq!(inflight_state.response_sent_offset, 0);
}

#[test]
fn handle_gemini_retry_boundary_clears_partial_output_and_local_session_state() {
    let mut full_response = "partial answer".to_string();
    let mut current_tool_line = Some("⚙ Bash: pwd".to_string());
    let mut last_tool_name = Some("Bash".to_string());
    let mut last_tool_summary = Some("pwd".to_string());
    let mut any_tool_used = true;
    let mut has_post_tool_text = true;
    let mut response_sent_offset = 42usize;
    let mut last_edit_text = "partial answer".to_string();
    let mut new_session_id = Some("stale".to_string());
    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Gemini,
        1479671298497183835,
        Some("adk-gm".to_string()),
        343742347365974026,
        1,
        2,
        "resume me".to_string(),
        Some("stale".to_string()),
        Some("AgentDesk-gemini-adk-gm".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        0,
    );
    inflight_state.full_response = full_response.clone();
    inflight_state.current_tool_line = current_tool_line.clone();
    inflight_state.any_tool_used = true;
    inflight_state.has_post_tool_text = true;
    inflight_state.response_sent_offset = response_sent_offset;

    let changed = handle_gemini_retry_boundary(
        &mut full_response,
        &mut current_tool_line,
        &mut last_tool_name,
        &mut last_tool_summary,
        &mut any_tool_used,
        &mut has_post_tool_text,
        &mut response_sent_offset,
        &mut last_edit_text,
        &mut new_session_id,
        &mut inflight_state,
    );

    assert!(changed);
    assert!(full_response.is_empty());
    assert_eq!(current_tool_line, None);
    assert_eq!(last_tool_name, None);
    assert_eq!(last_tool_summary, None);
    assert!(!any_tool_used);
    assert!(!has_post_tool_text);
    assert_eq!(response_sent_offset, 0);
    assert!(last_edit_text.is_empty());
    assert_eq!(new_session_id, None);
    assert_eq!(inflight_state.session_id, None);
    assert!(inflight_state.full_response.is_empty());
    assert_eq!(inflight_state.current_tool_line, None);
    assert!(!inflight_state.any_tool_used);
    assert!(!inflight_state.has_post_tool_text);
    assert_eq!(inflight_state.response_sent_offset, 0);
}

#[test]
fn stale_resume_result_helper_requires_error_result_record() {
    let assistant_text = serde_json::json!({
        "type": "assistant",
        "message": {
            "content": [{
                "type": "text",
                "text": "The log said No conversation found"
            }]
        }
    });
    let success_result = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "result": "No conversation found while inspecting logs",
    });
    let error_result = serde_json::json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "errors": ["No conversation found"],
    });

    assert!(!result_event_has_stale_resume_error(&assistant_text));
    assert!(!result_event_has_stale_resume_error(&success_result));
    assert!(result_event_has_stale_resume_error(&error_result));
}

#[test]
fn stale_resume_output_scan_ignores_assistant_mentions() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "I saw `No conversation found` in the logs."
                }]
            }
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "result": "analysis complete"
        })
    )
    .unwrap();
    file.flush().unwrap();

    assert!(!output_file_has_stale_resume_error_after_offset(
        file.path().to_str().unwrap(),
        0,
    ));
}

#[test]
fn stale_resume_output_scan_detects_error_result_after_offset() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "before"
                }]
            }
        })
    )
    .unwrap();
    file.flush().unwrap();
    let offset = std::fs::metadata(file.path()).unwrap().len();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["No conversation found"]
        })
    )
    .unwrap();
    file.flush().unwrap();

    assert!(output_file_has_stale_resume_error_after_offset(
        file.path().to_str().unwrap(),
        offset,
    ));
}

#[test]
fn explicit_review_verdict_parser_accepts_structured_marker() {
    assert_eq!(
        extract_explicit_review_verdict("VERDICT: pass\nNo findings."),
        Some("pass")
    );
    assert_eq!(
        extract_explicit_review_verdict("overall: improve\nNeeds work."),
        Some("improve")
    );
}

#[test]
fn explicit_review_verdict_parser_ignores_unstructured_text() {
    assert_eq!(
        extract_explicit_review_verdict("검토 완료. 전반적으로 좋아 보입니다."),
        None
    );
}

#[test]
fn review_decision_parser_accepts_explicit_marker() {
    assert_eq!(
        extract_review_decision("DECISION: accept\n리뷰 반영하겠습니다."),
        Some("accept")
    );
    assert_eq!(
        extract_review_decision("결정: dismiss\n이 리뷰는 무시합니다."),
        Some("dismiss")
    );
    assert_eq!(
        extract_review_decision("Decision: dispute\n반론을 제기합니다."),
        Some("dispute")
    );
}

#[test]
fn review_decision_parser_accepts_keyword_in_tail() {
    assert_eq!(
        extract_review_decision("리뷰 내용을 검토한 결과 수정이 필요합니다.\n\naccept"),
        Some("accept")
    );
    assert_eq!(
        extract_review_decision("불필요한 변경이므로 dismiss 합니다."),
        Some("dismiss")
    );
}

#[test]
fn review_decision_parser_rejects_ambiguous_keywords() {
    // Multiple different keywords -> ambiguous, return None
    assert_eq!(
        extract_review_decision("accept or dismiss 중 선택해야 합니다."),
        None
    );
}

#[test]
fn review_decision_parser_ignores_unstructured_text() {
    assert_eq!(
        extract_review_decision("리뷰 피드백을 확인했습니다. 코드를 수정하겠습니다."),
        None
    );
}

#[test]
fn review_decision_explicit_marker_takes_priority() {
    // Even with keywords in tail, explicit marker should be found first
    assert_eq!(
        extract_review_decision("DECISION: accept\n이 dismiss는 무시해도 됩니다."),
        Some("accept")
    );
}

#[test]
fn review_decision_parser_handles_korean_text_over_500_bytes() {
    // Korean chars are 3 bytes each in UTF-8; build a response > 500 bytes
    // to exercise the safe_suffix path without panicking
    let padding = "가".repeat(200); // 600 bytes of Korean text
    let response = format!("{padding}\ndismiss");
    assert_eq!(extract_review_decision(&response), Some("dismiss"));
}

#[test]
fn verdict_fallback_payload_includes_provider() {
    let payload = build_verdict_payload("d-123", "pass", "LGTM", "codex");
    assert_eq!(payload["dispatch_id"], "d-123");
    assert_eq!(payload["overall"], "pass");
    assert_eq!(payload["feedback"], "LGTM");
    assert_eq!(payload["provider"], "codex");
}

#[test]
fn verdict_fallback_payload_truncates_long_feedback() {
    let long_response = "x".repeat(5000);
    let payload = build_verdict_payload("d-456", "improve", &long_response, "claude");
    assert_eq!(payload["provider"], "claude");
    let feedback = payload["feedback"].as_str().unwrap();
    assert!(feedback.len() <= 4003); // 4000 + "..." ellipsis
}

// ========== resolve_done_response tests ==========

#[test]
fn done_replaces_stale_pre_tool_text_with_result() {
    // Text -> ToolUse -> Done(result): intermediate text should be replaced
    let res = resolve_done_response("이슈를 생성합니다.\n\n", "이슈 #90 생성 완료", true, false);
    assert_eq!(res, Some("이슈 #90 생성 완료".to_string()));
}

#[test]
fn done_keeps_full_response_when_post_tool_text_exists() {
    // Text -> ToolUse -> Text -> Done(result): streaming captured everything
    let res = resolve_done_response(
        "진행 중...\n\n이슈 #90 생성 완료",
        "이슈 #90 생성 완료",
        true,
        true,
    );
    assert_eq!(res, None); // keep full_response as-is
}

#[test]
fn done_uses_result_when_full_response_empty() {
    let res = resolve_done_response("", "최종 응답", false, false);
    assert_eq!(res, Some("최종 응답".to_string()));
}

#[test]
fn done_uses_result_when_full_response_whitespace_only() {
    let res = resolve_done_response("  \n\n  ", "최종 응답", true, false);
    assert_eq!(res, Some("최종 응답".to_string()));
}

#[test]
fn done_keeps_full_response_when_no_tools_used() {
    // Pure text turn without tools — streaming text IS the final response
    let res = resolve_done_response(
        "여기 분석 결과입니다...",
        "여기 분석 결과입니다...",
        false,
        false,
    );
    assert_eq!(res, None);
}

#[test]
fn done_noop_when_result_empty() {
    // Synthetic Done with empty result — nothing to replace with
    let res = resolve_done_response("중간 텍스트\n\n", "", true, false);
    assert_eq!(res, None);
}
