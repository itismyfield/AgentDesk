use super::*;
use serde_json::json;

#[test]
fn compact_summary_idle_transcript_scans_skip_summary_and_keep_human() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    let summary = format!(
        "{}\n",
        json!({"type":"user","uuid":"summary",
        "isCompactSummary":true,"isVisibleInTranscriptOnly":true,
        "message":{"role":"user","content":"평범한 요약 문장"}})
    );
    let human = format!(
        "{}\n",
        json!({"type":"user","uuid":"human",
        "message":{"role":"user","content":"실제 사용자 입력"}})
    );
    for scan in [
        scan_claude_idle_transcript_for_prompt,
        scan_claude_idle_transcript_for_last_prompt,
    ] {
        std::fs::write(&path, &summary).unwrap();
        assert_eq!(
            scan(&path, 0).unwrap(),
            ClaudeIdleTranscriptScan::NoPrompt {
                offset: summary.len() as u64,
            }
        );
        std::fs::write(&path, format!("{summary}{human}{summary}")).unwrap();
        assert_eq!(
            scan(&path, 0).unwrap(),
            ClaudeIdleTranscriptScan::Prompt {
                prompt: "실제 사용자 입력".into(),
                prompt_start_offset: summary.len() as u64,
                line_end_offset: (summary.len() + human.len()) as u64,
                entry_id: Some("human".into()),
                prompt_id: None,
            }
        );
    }
}

#[test]
fn compact_summary_idle_extraction_uses_only_boolean_summary_flag() {
    use crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id;
    for (flags, accepted) in [
        (json!({}), true),
        (json!({"isCompactSummary":false}), true),
        (json!({"isCompactSummary":"true"}), true),
        (json!({"isVisibleInTranscriptOnly":true}), true),
        (json!({"isSidechain":true}), true),
        (json!({"isMeta":true}), false),
        (json!({"isCompactSummary":true}), false),
    ] {
        let mut record = json!({"type":"user","uuid":"entry",
            "message":{"role":"user","content":"같은 본문"}});
        record
            .as_object_mut()
            .unwrap()
            .extend(flags.as_object().unwrap().clone());
        assert_eq!(
            extract_claude_transcript_user_prompt_with_entry_id(&record),
            accepted.then(|| ("같은 본문".into(), Some("entry".into()))),
            "{flags}"
        );
    }
}
