use super::*;
use serde_json::json;

#[test]
fn compact_summary_watcher_entrypoint_retains_both_sides() {
    for content in [json!("요약 대신 평범한 문장"), json!([])] {
        let mut buffer = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            json!({"type":"user","message":{"role":"user","content":"실제 입력"}}),
            json!({"type":"assistant","message":{"content":[
                {"type":"text","text":"BEFORE_ONE"}, {"type":"text","text":"BEFORE_TWO"}]}}),
            json!({"type":"system","subtype":"compact_boundary"}),
            json!({"type":"user","isCompactSummary":true,"isVisibleInTranscriptOnly":true,
                "isSidechain":false,"message":{"role":"user","content":content}}),
            json!({"type":"assistant","message":{"content":[
                {"type":"text","text":"AFTER_ONE"}, {"type":"text","text":"AFTER_TWO"}]}}),
            json!({"type":"system","subtype":"stop_hook_summary"})
        );
        let stop_offset = (buffer.len() - buffer.lines().last().unwrap().len() - 1) as u64;
        let mut body = String::new();
        let outcome = process_watcher_lines_for_turn(
            &mut buffer,
            &mut StreamLineState::new(),
            &mut body,
            &mut WatcherToolState::new(),
            Some(0),
            Some(0),
        );
        assert_eq!(
            outcome.terminal_kind,
            Some(WatcherTerminalKind::SoftStopHookSummary),
            "summary must not split the owned response; body={body:?}, unread={}",
            buffer.len()
        );
        assert_eq!(outcome.terminal_evidence_offset, Some(stop_offset));
        for text in ["BEFORE_ONE", "BEFORE_TWO", "AFTER_ONE", "AFTER_TWO"] {
            assert_eq!(body.matches(text).count(), 1, "{body:?}");
        }
        assert!(buffer.is_empty());
    }
}

#[test]
fn compact_summary_boundary_controls_preserve_human_and_meta_behavior() {
    for (flags, boundary) in [
        (json!({}), true),
        (json!({"isCompactSummary":false}), true),
        (json!({"isCompactSummary":"true"}), true),
        (json!({"isVisibleInTranscriptOnly":true}), true),
        (json!({"isSidechain":true}), true),
        (json!({"isMeta":true}), false),
        (json!({"isCompactSummary":true}), false),
    ] {
        let mut user = json!({"type":"user","message":{"role":"user","content":"같은 본문"}});
        user.as_object_mut()
            .unwrap()
            .extend(flags.as_object().unwrap().clone());
        let input = format!("{user}\n");
        let mut buffer = input.clone();
        let mut body = "existing response".to_owned();
        let outcome = process_watcher_lines_for_turn(
            &mut buffer,
            &mut StreamLineState::new(),
            &mut body,
            &mut WatcherToolState::new(),
            Some(50),
            Some(0),
        );
        assert_eq!(
            outcome.terminal_kind,
            boundary.then_some(WatcherTerminalKind::SoftUserBoundary),
            "{flags}"
        );
        assert_eq!(outcome.terminal_evidence_offset, boundary.then_some(50));
        assert_eq!(buffer, if boundary { input } else { String::new() });
        assert_eq!(body, "existing response");
    }
}
